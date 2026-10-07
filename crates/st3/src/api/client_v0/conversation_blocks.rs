//! Owner-local wire normalization. References encrypt source metadata and hold no durable
//! state: a fetch reads one authenticated native record and refuses edited/replaced content.
use super::*;
use crate::external_sessions::{ExternalConversation, ExternalSession};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::aead;
use std::io::Read as _;
use std::sync::LazyLock;

// Bound expensive reads without a queued backlog or a second content cache.
static READ_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
static REF_KEY: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let mut key = [0; 32];
    getrandom::fill(&mut key).expect("owner reference key entropy");
    key
});
fn read_slot(slots: &tokio::sync::Semaphore) -> Result<tokio::sync::SemaphorePermit<'_>, ApiError> {
    slots.try_acquire().map_err(|_| ApiError {
        status: StatusCode::TOO_MANY_REQUESTS,
        code: "rate-limited".into(),
        message: "owner conversation reads are busy; retry this read".into(),
        details: Box::default(),
    })
}

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
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&json!([
            source.driver.as_str(),
            source.native_id,
            source.transcript,
            identity
        ]))
        .map_err(ApiError::internal)?,
    )))
}

#[derive(Deserialize, Serialize)]
struct LocatedSource {
    driver: crate::external_sessions::ExternalDriver,
    native_id: String,
    transcript: std::path::PathBuf,
}

impl LocatedSource {
    fn from_session(source: &ExternalSession) -> Self {
        Self {
            driver: source.driver,
            native_id: source.native_id.clone(),
            transcript: source.transcript.clone(),
        }
    }

    fn session(&self, id: &str) -> ExternalSession {
        ExternalSession {
            id: id.into(),
            revision: String::new(),
            driver: self.driver,
            native_id: self.native_id.clone(),
            transcript: self.transcript.clone(),
            codex_home: None,
            cwd: None,
            title: None,
            started_at_unix_ms: 0,
            updated_at_unix_ms: 0,
            process: None,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct ContentLocator {
    source: LocatedSource,
    basis: String,
    session: String,
    entry: Value,
    revision: Value,
    pointer: String,
    image: bool,
    native: Value,
}

fn reference(
    source: &ExternalSession,
    basis: &str,
    session: &str,
    item: &Value,
    pointer: &str,
) -> String {
    let locator = ContentLocator {
        source: LocatedSource::from_session(source),
        basis: basis.into(),
        session: session.into(),
        entry: item["id"].clone(),
        revision: item["revision"].clone(),
        pointer: pointer.into(),
        image: item
            .pointer(pointer)
            .is_some_and(|value| image(value) && !external_image(value)),
        native: item["_source"].clone(),
    };
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &*REF_KEY).expect("owner reference key"),
    );
    let mut nonce = [0; aead::NONCE_LEN];
    getrandom::fill(&mut nonce).expect("owner reference nonce entropy");
    let mut encrypted = serde_json::to_vec(&locator).expect("JSON value encodes");
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(b"st-conversation-ref-v2"),
        &mut encrypted,
    )
    .expect("owner reference encryption");
    format!(
        "v2.{}",
        URL_SAFE_NO_PAD.encode([nonce.as_slice(), &encrypted].concat())
    )
}

fn locator(wanted: &str, session: &str) -> Result<ContentLocator, ApiError> {
    if wanted.len() > 4096 {
        return Err(invalidated());
    }
    let encoded = wanted.strip_prefix("v2.").ok_or_else(invalidated)?;
    let mut encrypted = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| invalidated())?;
    if encrypted.len() < aead::NONCE_LEN + aead::CHACHA20_POLY1305.tag_len() {
        return Err(invalidated());
    }
    let (nonce, payload) = encrypted.split_at_mut(aead::NONCE_LEN);
    let nonce: [u8; aead::NONCE_LEN] = nonce.try_into().map_err(|_| invalidated())?;
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &*REF_KEY).expect("owner reference key"),
    );
    let plaintext = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(b"st-conversation-ref-v2"),
            payload,
        )
        .map_err(|_| invalidated())?;
    let locator: ContentLocator = serde_json::from_slice(plaintext).map_err(|_| invalidated())?;
    if locator.session != session
        || locator.pointer.len() > 512
        || !(locator.pointer == "/body" || locator.pointer.starts_with("/body/"))
    {
        return Err(invalidated());
    }
    Ok(locator)
}

fn located_value(source: &ExternalSession, locator: &ContentLocator) -> Result<Value, ApiError> {
    if basis(source)? != locator.basis {
        return Err(invalidated());
    }
    let native = serde_json::from_value(locator.native.clone()).map_err(|_| invalidated())?;
    crate::external_sessions::normalized_record(source, &native)
        .map_err(|_| invalidated())?
        .into_iter()
        .find(|item| item["id"] == locator.entry && item["revision"] == locator.revision)
        .and_then(|item| item.pointer(&locator.pointer).cloned())
        .ok_or_else(invalidated)
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

/// Bound an open object's values without replacing its keys or its outer object.
/// Callers attach a continuation to the original subtree, before this display-only edit.
pub(crate) fn bound_open_value(value: &mut Value) -> bool {
    match value {
        Value::String(text) if text.len() > VALUE_BYTES => {
            *text = clipped(text);
            true
        }
        Value::Object(values) => {
            let mut changed = false;
            for value in values.values_mut() {
                changed |= bound_open_value(value);
                // Leave room for the visible marker added to an 8 KiB string. Large
                // collections of small values still need a bounded JSON preview.
                if matches!(value, Value::Object(_) | Value::Array(_)) {
                    let encoded = serde_json::to_string(value).expect("JSON value encodes");
                    if encoded.len() > VALUE_BYTES * 2 {
                        *value = json!(clipped(&encoded));
                        changed = true;
                    }
                }
            }
            changed
        }
        Value::Array(values) => {
            let mut changed = false;
            for value in values.iter_mut() {
                changed |= bound_open_value(value);
            }
            let encoded = serde_json::to_string(values).expect("JSON value encodes");
            if encoded.len() > VALUE_BYTES * 2 {
                *value = json!(clipped(&encoded));
                changed = true;
            }
            changed
        }
        _ => false,
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

fn image_media(_value: &Value) -> &str {
    // Native MIME labels are untrusted. Fetch reports the detected passive image format.
    "application/octet-stream"
}

fn external_image(value: &Value) -> bool {
    image_source(value)
        .is_some_and(|source| source.starts_with("https://") || source.starts_with("http://"))
}

fn image_source(value: &Value) -> Option<&str> {
    value
        .get("data")
        .or_else(|| value.pointer("/source/data"))
        .or_else(|| value.get("image_url").filter(|value| value.is_string()))
        .or_else(|| value.pointer("/image_url/url"))
        .or_else(|| value.pointer("/source/url"))
        .or_else(|| value.get("url"))
        .and_then(Value::as_str)
}

fn continuation(reference: String, media: &str, size: Option<usize>, reason: &str) -> Value {
    let mut result = json!({"ref":reference,"media_type":media,"reason":reason});
    if let Some(size) = size {
        result["size"] = json!(size);
    }
    result
}

fn image_refs(
    value: &mut Value,
    pointer: &str,
    source: &ExternalSession,
    basis: &str,
    session: &str,
    item: &Value,
) {
    if image(value) {
        if external_image(value) {
            return;
        }
        *value = json!({"type":"image","content":continuation(reference(source,basis,session,item,pointer),image_media(value),None,"on-demand")});
        return;
    }
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                image_refs(
                    value,
                    &format!("{pointer}/{index}"),
                    source,
                    basis,
                    session,
                    item,
                );
            }
        }
        Value::Object(values) => {
            for (key, value) in values.iter_mut() {
                image_refs(
                    value,
                    &format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1")),
                    source,
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
    let _slot = read_slot(&READ_SLOTS)?;
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
                let body_ref = block["payload"] == json!({"body_ref":true});
                let pointer = if body_ref {
                    "/body".to_owned()
                } else {
                    format!("/body/blocks/{index}/payload")
                };
                if body_ref {
                    block["payload"] = original["body"].clone();
                    block["payload"]
                        .as_object_mut()
                        .expect("native body")
                        .remove("blocks");
                }
                if block["kind"] == "image" && external_image(&block["payload"]) {
                    block["kind"] = json!("image_link");
                } else if block["kind"] == "image" {
                    block["continuation"] = continuation(
                        reference(source, &basis, session_id, &original, &pointer),
                        image_media(&block["payload"]),
                        None,
                        "on-demand",
                    );
                    block["payload"] = json!({});
                } else {
                    image_refs(
                        &mut block["payload"],
                        &pointer,
                        source,
                        &basis,
                        session_id,
                        &original,
                    );
                    let encoded =
                        serde_json::to_vec(&block["payload"]).map_err(ApiError::internal)?;
                    if encoded.len() > VALUE_BYTES || original["_oversized_bytes"].is_number() {
                        block["continuation"] = continuation(
                            reference(source, &basis, session_id, &original, &pointer),
                            "application/json",
                            Some(
                                original["_oversized_payload_bytes"][index]
                                    .as_u64()
                                    .map_or(encoded.len(), |size| size as usize),
                            ),
                            "size-limit",
                        );
                        bound(&mut block["payload"]);
                    }
                }
                if let Some(metadata) = block.get_mut("metadata")
                    && bound_open_value(metadata)
                    && block.get("continuation").is_none()
                {
                    let pointer = format!("/body/blocks/{index}/metadata");
                    block["continuation"] = continuation(
                        reference(source, &basis, session_id, &original, &pointer),
                        "application/json",
                        Some(
                            serde_json::to_vec(
                                original.pointer(&pointer).expect("native metadata"),
                            )
                            .map_err(ApiError::internal)?
                            .len(),
                        ),
                        "size-limit",
                    );
                }
            }
        }
        if let Some(block) = body
            .get("blocks")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            && block["kind"] == "unknown"
        {
            let label = body
                .get("text")
                .and_then(Value::as_str)
                .and_then(|text| text.lines().next())
                .unwrap_or("[unknown]");
            body["text"] = json!(format!("{label}\n{}", block["payload"]));
        }
        if let Some(block) = body
            .get("blocks")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            && block["kind"] == "image_link"
        {
            body["text"] = json!(format!(
                "[external image link · open explicitly]\n{}",
                block["payload"]
            ));
        }
        // Fallback bodies use known v0 types and also move native pixels to owner fetch refs.
        for key in ["text", "arguments", "content"] {
            if let Some(value) = body.get_mut(key) {
                image_refs(
                    value,
                    &format!("/body/{key}"),
                    source,
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
        // Error/status fallback fields use the same clipped display convention as
        // text/arguments/content. A negotiated body_ref continuation fetches the
        // complete original body; legacy clients receive the visible marker only.
        for key in ["message", "details", "detail"] {
            if let Some(value) = body.get_mut(key) {
                bound_open_value(value);
            }
        }
        let mut fallback = original["body"].clone();
        fallback
            .as_object_mut()
            .expect("native body")
            .remove("blocks");
        if let Some(blocks) = body.get_mut("blocks").and_then(Value::as_array_mut) {
            for (index, block) in blocks.iter_mut().enumerate() {
                if original["body"]["blocks"][index]["payload"] == fallback
                    || original["body"]["blocks"][index]["payload"] == json!({"body_ref":true})
                {
                    block["payload"] = json!({"body_ref": true});
                }
            }
        }
        item.as_object_mut().expect("native item").remove("_source");
        item.as_object_mut()
            .expect("native item")
            .remove("_oversized_bytes");
        item.as_object_mut()
            .expect("native item")
            .remove("_oversized_payload_bytes");
        if serde_json::to_vec(item).map_err(ApiError::internal)?.len()
            > CLIENT_MAX_RESPONSE_BYTES - 128_000
        {
            let body = item["body"].as_object_mut().expect("native body");
            for (key, value) in body.iter_mut() {
                if key != "blocks" {
                    bound_open_value(value);
                    if !value.is_string() {
                        bound(value);
                    }
                }
            }
            // Retained open-object keys can still exceed the transport budget.
            // Replace only this entry, preserving its identity and ordering.
            let size = serde_json::to_vec(item).map_err(ApiError::internal)?.len();
            if size > CLIENT_MAX_RESPONSE_BYTES - 128_000 {
                if item["type"] != "error" {
                    item["type"] = json!("error");
                    item["role"] = json!("system");
                }
                item["body"] = json!({
                    "code":"native-entry-too-large",
                    "message":format!("[st truncated this native timeline value: size limit; {size} bytes]"),
                    "retryable":false,
                    "details":{"size":size},
                    "blocks":[{
                        "id":"native-entry-too-large",
                        "kind":"error",
                        "source_type":"native-entry-too-large",
                        "payload":{"body_ref":true},
                        "continuation":continuation(
                            reference(source, &basis, session_id, &original, "/body"),
                            "application/json",
                            Some(serde_json::to_vec(&original["body"]).map_err(ApiError::internal)?.len()),
                            "size-limit",
                        )
                    }]
                });
            }
        }
        if !session.conversation_blocks {
            item["body"]
                .as_object_mut()
                .expect("native body")
                .remove("blocks");
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
    let locator = locator(wanted, session_id)?;
    let _slot = read_slot(&READ_SLOTS)?;
    let request_state = state.clone();
    let request_session = session_id.to_owned();
    let (bytes, media) = crate::api::read_deadline::spawn_blocking(move || -> Result<_, ApiError> {
        let native = request_session.starts_with("session/external-");
        let source = if native {
            locator.source.session(&request_session)
        } else {
            source(&request_state, &request_session)?
        };
        let value = located_value(&source, &locator)?;
        let result = if locator.image {
            image_bytes(&source, &value)?
        } else {
            (
                serde_json::to_vec(&value).map_err(ApiError::internal)?,
                "application/json".to_owned(),
            )
        };
        // Bytes belong to the validated record snapshot. Blob hashes bind external
        // pixels to that snapshot. Recheck source identity/binding, without decoding
        // the same record twice. An edit after capture is observed by the next fetch.
        let rebound = if native {
            source
        } else {
            self::source(&request_state, &request_session)?
        };
        if basis(&rebound)? != locator.basis {
            return Err(invalidated());
        }
        Ok(result)
    })
    .await
    .map_err(ApiError::internal)??;
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

fn image_bytes(source: &ExternalSession, value: &Value) -> Result<(Vec<u8>, String), ApiError> {
    let encoded = image_source(value)
        .ok_or_else(|| transcript_unavailable("native image has no readable source"))?;
    let bytes = if let Some(uri) = encoded.strip_prefix("data:") {
        let (header, body) = uri
            .split_once(',')
            .ok_or_else(|| validation("native image data URI is malformed"))?;
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
        let root = blob_root(source)?;
        let bytes = read_blob(&root, &root.join(digest))?;
        if hex::encode(Sha256::digest(&bytes)) != digest {
            return Err(invalidated());
        }
        bytes
    } else if let Some(path) = encoded.strip_prefix("file://") {
        let decoded =
            urlencoding::decode(path).map_err(|_| validation("native file URI is malformed"))?;
        let root = blob_root(source)?;
        let path = std::path::Path::new(decoded.as_ref());
        let digest = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| {
                validation("native file URI must name a content-addressed harness blob")
            })?;
        let bytes = read_blob(&root, path)?;
        if hex::encode(Sha256::digest(&bytes)) != digest {
            return Err(invalidated());
        }
        bytes
    } else if encoded.starts_with("https://") || encoded.starts_with("http://") {
        return Err(transcript_unavailable(
            "external image link: open it explicitly in the client; the owner does not fetch URLs",
        ));
    } else {
        STANDARD
            .decode(encoded)
            .map_err(|_| validation("native image base64 is malformed"))?
    };
    if bytes.len() > IMAGE_BYTES {
        return Err(validation("native image exceeds the 32 MiB read limit"));
    }
    let media = detected_media(&bytes).to_owned();
    Ok((bytes, media))
}

fn blob_root(source: &ExternalSession) -> Result<std::path::PathBuf, ApiError> {
    if !matches!(
        source.driver,
        crate::external_sessions::ExternalDriver::Omp
            | crate::external_sessions::ExternalDriver::Pi
    ) {
        return Err(transcript_unavailable("native blob store is not bound"));
    }
    let root = source
        .transcript
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "sessions"))
        .and_then(|path| path.parent())
        .ok_or_else(|| transcript_unavailable("native blob store is not bound"))?
        .join("blobs");
    let metadata = std::fs::symlink_metadata(&root)
        .map_err(|_| transcript_unavailable("native blob store is missing"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(validation(
            "native blob store must be a bound directory, not a symlink",
        ));
    }
    let parent = root
        .parent()
        .expect("blob store parent")
        .canonicalize()
        .map_err(ApiError::internal)?;
    let canonical = root.canonicalize().map_err(ApiError::internal)?;
    if canonical.parent() != Some(parent.as_path()) {
        return Err(validation(
            "native blob store escaped its bound provider home",
        ));
    }
    Ok(canonical)
}

fn read_blob(root: &std::path::Path, path: &std::path::Path) -> Result<Vec<u8>, ApiError> {
    let canonical = path
        .canonicalize()
        .map_err(|_| transcript_unavailable("native image blob is missing"))?;
    if canonical.parent() != Some(root) {
        return Err(validation(
            "native image path is outside the bound harness blob store",
        ));
    }
    let name = canonical
        .file_name()
        .ok_or_else(|| validation("native image path is invalid"))?;
    #[cfg(unix)]
    let file = {
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .map_err(ApiError::internal)?;
        let name = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| validation("native image path is invalid"))?;
        // The open directory pins the store, and O_NOFOLLOW refuses last-moment symlink swaps.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(transcript_unavailable(
                "native image blob could not be opened",
            ));
        }
        unsafe { std::fs::File::from_raw_fd(fd) }
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(root.join(name)).map_err(ApiError::internal)?;
    if !file.metadata().map_err(ApiError::internal)?.is_file() {
        return Err(validation("native image blob is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take((IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ApiError::internal)?;
    Ok(bytes)
}

fn detected_media(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "application/octet-stream"
    } // SVG/HTML and unrecognized bytes never get an active image MIME.
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

    async fn fetch_json_chunks(state: &AppState, native: &ExternalSession, token: &str) -> Value {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt as _;
        let app = super::super::super::router(state.clone());
        let mut bytes = Vec::new();
        let mut offset = 0;
        loop {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/v1/client/conversations/{}/content/{token}/chunk?offset={offset}",
                            native.id.trim_start_matches("session/")
                        ))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let chunk: Value = serde_json::from_slice(
                &to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
                    .await
                    .unwrap(),
            )
            .unwrap();
            // HTTP routes wrap their result in the client envelope; chunk_local
            // alone returns the bare content-chunk object.
            let chunk = &chunk["value"];
            bytes.extend(STANDARD.decode(chunk["data"].as_str().unwrap()).unwrap());
            let Some(next) = chunk["next_offset"].as_u64() else {
                break;
            };
            offset = next;
        }
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn open_tool_metadata_fits_pages_and_socket_frames_and_fetches_exact_native_values() {
        use futures_util::{SinkExt as _, StreamExt as _};
        let root = tempfile::tempdir().unwrap();
        let details = json!({
            "wallTimeMs": 12.75,
            "future": {"errorMessage": "é".repeat(1024 * 1024)},
            "future/key~": "unchanged"
        });
        let native = fixture(
            root.path(),
            json!([{"type":"toolResult", "call_id":"large-call", "content":"ok", "details":details}]),
        );
        let mut state = super::super::tests::test_state_named(root.path(), "metadata-owner");
        state.native_session_home = Some(root.path().to_path_buf());
        for negotiated in [true, false] {
            let mut session = ClientSession::local(None).unwrap();
            session.conversation_blocks = negotiated;
            let page = read(&native, &session, &native.id).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
            let item = page
                .iter()
                .find(|item| item["type"] == "tool_result")
                .unwrap();
            assert!(serde_json::to_vec(item).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
            if negotiated {
                let block = &item["body"]["blocks"][0];
                assert_eq!(block["metadata"]["wallTimeMs"], details["wallTimeMs"]);
                assert_eq!(block["metadata"]["future/key~"], "unchanged");
                assert!(
                    block["metadata"]["future"]["errorMessage"]
                        .as_str()
                        .unwrap()
                        .contains(
                            "[st truncated this native timeline value: size limit; 2097152 bytes]"
                        )
                );
                let token = block["continuation"]["ref"].as_str().unwrap();
                assert_eq!(
                    locator(token, &native.id).unwrap().pointer,
                    "/body/blocks/0/metadata"
                );
                assert_eq!(fetch_json_chunks(&state, &native, token).await, details);
            } else {
                assert!(item["body"].get("blocks").is_none());
                let _: Vec<st3_client::TimelineEntry> =
                    serde_json::from_value(json!(page)).unwrap();
            }
            let socket_state = state.clone();
            let app = axum::Router::new().route(
                "/stream",
                axum::routing::get(move |upgrade: WebSocketUpgrade| {
                    let (state, session) = (socket_state.clone(), session.clone());
                    async move {
                        upgrade.on_upgrade(move |socket| {
                            super::super::collection_stream_socket_with_reader(
                                socket,
                                state,
                                session,
                                None,
                                |state, session, request, permit| async move {
                                    super::super::collection_items(
                                        &state, &session, &request, permit,
                                    )
                                    .await
                                },
                            )
                        })
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let (mut socket, _) =
                tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
                    .await
                    .unwrap();
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    json!({
                        "kind":"subscribe",
                        "id":"metadata",
                        "collection":"conversation",
                        "conversation":native.id
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
                panic!("expected bounded conversation snapshot, got {frame:?}");
            };
            assert!(text.len() < CLIENT_MAX_RESPONSE_BYTES);
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["kind"], "conversation");
            assert_eq!(value["replace"], true);
            let items = value["items"].as_array().unwrap();
            assert!(
                items.iter().any(|item| item["type"] == "tool_result"),
                "frame bytes={}, entry types/sizes={:?}",
                text.len(),
                items
                    .iter()
                    .map(|item| (&item["type"], serde_json::to_vec(item).unwrap().len()))
                    .collect::<Vec<_>>()
            );
            socket.close(None).await.unwrap();
            server.abort();
        }
    }

    #[tokio::test]
    async fn native_omp_outcomes_are_bounded_on_http_and_socket_and_reconstruct_exact_bodies() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use futures_util::{SinkExt as _, StreamExt as _};
        use tower::ServiceExt as _;

        fn assert_bounded_outcomes(items: &[Value], negotiated: bool) -> Vec<&Value> {
            let mut outcomes = Vec::new();
            for (entry_type, code) in [
                ("error", Some("native_provider_error")),
                ("status", None),
                ("error", Some("native_session_exit_fatal")),
                ("error", Some("native_session_exit_unknown")),
            ] {
                let item = items
                    .iter()
                    .find(|item| {
                        item["type"] == entry_type
                            && code.is_none_or(|code| item["body"]["code"] == code)
                    })
                    .unwrap();
                assert_eq!(item["role"], "system");
                assert!(serde_json::to_vec(item).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
                let body = &item["body"];
                if code == Some("native_provider_error") {
                    for value in [&body["message"], &body["details"]["errorMessage"]] {
                        assert!(
                            value.as_str().unwrap().contains(
                                "[st truncated this native timeline value: size limit; 614400 bytes]"
                            )
                        );
                    }
                    assert_eq!(body["details"]["errorStatus"], 429);
                } else {
                    let detail = if code.is_some() {
                        &body["details"]["pendingToolCalls"]
                    } else {
                        assert_eq!(body["status"], "completed");
                        &body["detail"]
                    };
                    assert!(detail.as_str().unwrap().contains("size limit"));
                    assert!(detail.as_str().unwrap().contains("[st truncated"));
                }
                if negotiated {
                    let block = body["blocks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|block| block["kind"] == entry_type)
                        .unwrap();
                    assert_eq!(block["continuation"]["reason"], "size-limit");
                } else {
                    assert!(body.get("blocks").is_none());
                }
                outcomes.push(item);
            }
            let _: Vec<st3_client::TimelineEntry> =
                serde_json::from_value(json!(items)).unwrap();
            outcomes
        }

        let root = tempfile::tempdir().unwrap();
        let native = fixture(root.path(), json!([]));
        let provider_message = json!({
            "role":"assistant","content":[],"stopReason":"error",
            "errorMessage":"p".repeat(600 * 1024),"errorStatus":429,
            "errorId":{"native":"provider-error"},
            "retryRecovery":{"attempt":2,"future":[false,null,42]},
        });
        let pending = json!((0..20).map(|index| json!({
            "toolCallId":format!("pending-{index}"),"toolName":"shell",
            "args":{"command":"x".repeat(32_768)},"intent":"native intent",
        })).collect::<Vec<_>>());
        let exits = [
            json!({"kind":"signal","reason":"sigterm","pendingToolCalls":pending,"future":42}),
            json!({"kind":"fatal","reason":"uncaught_exception","pendingToolCalls":pending,"future":42}),
            json!({"kind":"future-kind","reason":"unknown","pendingToolCalls":pending,"future":42}),
        ];
        let records = [
            json!({"type":"session","id":"native-test","cwd":"/work/example","timestamp":"2026-10-06T12:00:00Z"}),
            json!({"type":"message","id":"provider","timestamp":"2026-10-06T12:00:01Z","message":provider_message}),
            json!({"type":"custom","id":"signal","timestamp":"2026-10-06T12:00:02Z","customType":"session_exit","data":exits[0]}),
            json!({"type":"custom","id":"fatal","timestamp":"2026-10-06T12:00:03Z","customType":"session_exit","data":exits[1]}),
            json!({"type":"custom","id":"unknown","timestamp":"2026-10-06T12:00:04Z","customType":"session_exit","data":exits[2]}),
        ];
        std::fs::write(
            &native.transcript,
            records.iter().map(|record| format!("{record}\n")).collect::<String>(),
        )
        .unwrap();
        let original = crate::external_sessions::normalized_timeline(&native).unwrap();
        let mut state = super::super::tests::test_state_named(root.path(), "outcomes-owner");
        state.native_session_home = Some(root.path().to_path_buf());
        let app = super::super::super::router(state.clone());
        for negotiated in [true, false] {
            let mut request = Request::builder().uri(format!(
                "/v1/client/sessions/{}/timeline",
                native.id.trim_start_matches("session/")
            ));
            if negotiated {
                request = request.header("x-st3-features", "conversation-blocks.v1");
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
                .await
                .unwrap();
            assert!(bytes.len() < CLIENT_MAX_RESPONSE_BYTES);
            let page: Value = serde_json::from_slice(&bytes).unwrap();
            let items = page["value"]["items"].as_array().unwrap();
            let outcomes = assert_bounded_outcomes(items, negotiated);
            if negotiated {
                for (index, item) in outcomes.iter().enumerate() {
                    let block = item["body"]["blocks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|block| block["kind"] == item["type"])
                        .unwrap();
                    let token = block["continuation"]["ref"].as_str().unwrap();
                    assert_eq!(locator(token, &native.id).unwrap().pointer, "/body");
                    let full = fetch_json_chunks(&state, &native, token).await;
                    let source = original.iter().find(|source| source["id"] == item["id"]).unwrap();
                    assert_eq!(full, source["body"]);
                    match index {
                        0 => {
                            assert_eq!(full["message"], provider_message["errorMessage"]);
                            assert_eq!(full["details"]["errorMessage"], provider_message["errorMessage"]);
                            assert_eq!(full["details"]["retryRecovery"], provider_message["retryRecovery"]);
                        }
                        1 => {
                            let detail: Value = serde_json::from_str(full["detail"].as_str().unwrap()).unwrap();
                            assert_eq!(detail, exits[0]);
                        }
                        _ => assert_eq!(full["details"], exits[index - 1]),
                    }
                }
            }
            let mut session = ClientSession::local(None).unwrap();
            session.conversation_blocks = negotiated;
            let socket_state = state.clone();
            let socket_app = axum::Router::new().route(
                "/stream",
                axum::routing::get(move |upgrade: WebSocketUpgrade| {
                    let (state, session) = (socket_state.clone(), session.clone());
                    async move {
                        upgrade.on_upgrade(move |socket| {
                            super::super::collection_stream_socket_with_reader(
                                socket,
                                state,
                                session,
                                None,
                                |state, session, request, permit| async move {
                                    super::super::collection_items(
                                        &state, &session, &request, permit,
                                    )
                                    .await
                                },
                            )
                        })
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, socket_app).await.unwrap();
            });
            let (mut socket, _) =
                tokio_tungstenite::connect_async(format!("ws://{address}/stream"))
                    .await
                    .unwrap();
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    json!({
                        "kind":"subscribe","id":"outcomes","collection":"conversation",
                        "conversation":native.id
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let tokio_tungstenite::tungstenite::Message::Text(text) = frame else {
                panic!("expected bounded conversation snapshot, got {frame:?}");
            };
            assert!(text.len() < CLIENT_MAX_RESPONSE_BYTES);
            let frame: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(frame["kind"], "conversation");
            assert_eq!(frame["replace"], true);
            let socket_outcomes =
                assert_bounded_outcomes(frame["items"].as_array().unwrap(), negotiated);
            for (socket_item, http_item) in socket_outcomes.iter().zip(&outcomes) {
                assert_eq!(socket_item["body"], http_item["body"]);
            }
            socket.close(None).await.unwrap();
            server.abort();
        }
    }

    #[test]
    fn open_values_preserve_outer_keys_but_bound_collections_of_small_values() {
        let mut value = json!({
            "keep": 42,
            "nested": {"many": (0..4000).map(|_| "small").collect::<Vec<_>>()},
            "slash/key~": "s".repeat(2 * VALUE_BYTES)
        });
        assert!(bound_open_value(&mut value));
        assert_eq!(value["keep"], 42);
        assert!(
            value["nested"]["many"]
                .as_str()
                .unwrap()
                .contains("truncated")
        );
        assert!(value["slash/key~"].as_str().unwrap().contains("truncated"));
        assert!(serde_json::to_vec(&value).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
    }

    #[test]
    fn total_entry_guard_bounds_future_body_fields_before_transport() {
        let root = tempfile::tempdir().unwrap();
        let native = fixture(
            root.path(),
            json!([{"type":"text", "text":"native fixture"}]),
        );
        for negotiated in [true, false] {
            let mut session = ClientSession::local(None).unwrap();
            session.conversation_blocks = negotiated;
            let mut items = crate::external_sessions::normalized_timeline(&native).unwrap();
            items[0]["body"]["future_body_field"] = json!("f".repeat(2 * 1024 * 1024));
            let page = prepare(&native, &session, &native.id, items).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
            assert!(
                page[0]["body"]["future_body_field"]
                    .as_str()
                    .unwrap()
                    .contains("size limit; 2097152 bytes")
            );
        }
    }

    #[tokio::test]
    async fn pathological_metadata_replaces_only_its_entry_and_preserves_http_page_and_owner_fetch()
    {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use tower::ServiceExt as _;
        let root = tempfile::tempdir().unwrap();
        let details = Value::Object(
            (0..200_000)
                .map(|index| (format!("key-{index}"), json!(0)))
                .collect(),
        );
        let native = fixture(
            root.path(),
            json!([
                {"type":"text", "text":"before pathological entry"},
                {"type":"toolResult", "call_id":"pathological", "content":"ok", "details":details},
                {"type":"text", "text":"after pathological entry"}
            ]),
        );
        let original = crate::external_sessions::normalized_timeline(&native).unwrap();
        let original_entry = original
            .iter()
            .find(|item| item["type"] == "tool_result")
            .unwrap();
        let mut state = super::super::tests::test_state_named(root.path(), "pathological-owner");
        state.native_session_home = Some(root.path().to_path_buf());
        let app = super::super::super::router(state.clone());
        for negotiated in [true, false] {
            let mut request = Request::builder().uri(format!(
                "/v1/client/sessions/{}/timeline",
                native.id.trim_start_matches("session/")
            ));
            if negotiated {
                request = request.header("x-st3-features", "conversation-blocks.v1");
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
                .await
                .unwrap();
            assert!(bytes.len() < CLIENT_MAX_RESPONSE_BYTES);
            let page: Value = serde_json::from_slice(&bytes).unwrap();
            let items = page["value"]["items"].as_array().unwrap();
            assert!(
                items
                    .iter()
                    .any(|item| item["body"]["text"] == "before pathological entry")
            );
            assert!(
                items
                    .iter()
                    .any(|item| item["body"]["text"] == "after pathological entry")
            );
            let marker = items
                .iter()
                .find(|item| item["body"]["code"] == "native-entry-too-large")
                .unwrap();
            for key in ["id", "sequence", "revision", "timestamp", "final"] {
                assert_eq!(marker[key], original_entry[key]);
            }
            assert_eq!(marker["type"], "error");
            assert_eq!(marker["role"], "system");
            assert_eq!(marker["body"]["retryable"], false);
            assert!(
                marker["body"]["details"]["size"].as_u64().unwrap()
                    > CLIENT_MAX_RESPONSE_BYTES as u64
            );
            assert!(
                marker["body"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("size limit")
            );
            if negotiated {
                let token = marker["body"]["blocks"][0]["continuation"]["ref"]
                    .as_str()
                    .unwrap();
                assert_eq!(locator(token, &native.id).unwrap().pointer, "/body");
                let full = fetch_json_chunks(&state, &native, token).await;
                assert_eq!(full["blocks"][0]["metadata"], details);
            } else {
                assert!(marker["body"].get("blocks").is_none());
                let _: Vec<st3_client::TimelineEntry> =
                    serde_json::from_value(json!(items)).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn full_unknown_arguments_reasoning_and_pixels_survive_owner_fetch_without_storage() {
        let root = tempfile::tempdir().unwrap();
        let raw =
            json!({"type":"future","nested":{"token":"invented-token","large":"é".repeat(10000)}});
        let mut pixels = vec![7u8; CHUNK_BYTES + 123];
        pixels[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
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
        assert!(
            blocks
                .iter()
                .any(|block| block["kind"] == "source_record" && block["visibility"] == "internal")
        );
        let blocks = blocks
            .into_iter()
            .filter(|block| block["kind"] != "source_record")
            .collect::<Vec<_>>();
        assert_eq!(
            blocks
                .iter()
                .map(|block| block["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["reasoning", "tool_call", "unknown", "image"]
        );
        assert_eq!(blocks[1]["payload"], json!({"body_ref":true}));
        assert_eq!(
            items
                .iter()
                .find(|item| item["type"] == "tool_call")
                .unwrap()["body"]["arguments"]["nested"]["value"],
            "full"
        );
        assert_eq!(blocks[2]["continuation"]["reason"], "size-limit");
        assert!(blocks[2]["payload"].as_str().unwrap().contains("truncated"));
        assert_eq!(blocks[3]["payload"], json!({}));
        assert!(
            !serde_json::to_string(&items)
                .unwrap()
                .contains(&STANDARD.encode(&pixels))
        );
        assert!(serde_json::to_vec(&items).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
        let original = crate::external_sessions::normalized_timeline(&source).unwrap();
        let location = locator(
            blocks[2]["continuation"]["ref"].as_str().unwrap(),
            &source.id,
        )
        .unwrap();
        assert_eq!(
            located_value(&source, &location).unwrap(),
            json!({"raw":raw})
        );
        let (decoded, mime) = image_bytes(
            &source,
            &original.last().unwrap()["body"]["blocks"][0]["payload"],
        )
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
        let mut pixels = vec![11u8; CHUNK_BYTES + 101];
        pixels[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
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
        // Native appends between chunks preserve both image and unknown-block refs.
        use std::io::Write as _;
        writeln!(std::fs::OpenOptions::new().append(true).open(&native.transcript).unwrap(), "{}", json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"appended while fetching"}]}})).unwrap();
        let (status, grown) = request(format!("/v1/client/sessions/{id}/timeline"), true).await;
        assert_eq!(status, StatusCode::OK, "{grown}");
        assert!(
            grown["value"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["body"]["text"] == "appended while fetching")
        );
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
        let token = reference(&source, &before, &source.id, item, "/body/blocks/0/payload");
        assert_ne!(
            token,
            reference(
                &source,
                &before,
                "session/other",
                item,
                "/body/blocks/0/payload"
            )
        );
        let mut revised = item.clone();
        revised["revision"] = json!(2);
        assert_ne!(
            token,
            reference(
                &source,
                &before,
                &source.id,
                &revised,
                "/body/blocks/0/payload"
            )
        );
        let location = locator(&token, &source.id).unwrap();
        assert!(locator(&token, "session/other").is_err());
        let mut forged = token.clone();
        forged.push('x');
        assert!(locator(&forged, &source.id).is_err());
        std::fs::write(&source.transcript, "replacement\n").unwrap();
        assert_eq!(
            before,
            basis(&source).unwrap(),
            "same inode, but entry digest must refuse the edit"
        );
        assert!(located_value(&source, &location).is_err());
        std::fs::rename(&source.transcript, source.transcript.with_extension("old")).unwrap();
        std::fs::write(&source.transcript, "replacement\n").unwrap();
        assert_ne!(before, basis(&source).unwrap());
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
    async fn native_chunks_use_encrypted_source_without_session_discovery() {
        let root = tempfile::tempdir().unwrap();
        let raw = json!({"type":"future","content":"invented-token".repeat(1000)});
        let native = fixture(root.path(), json!([raw.clone()]));
        let state = super::super::tests::test_state_named(root.path(), "direct-owner-test");
        // With discovery disabled, source() cannot find even this session. An issued ref
        // must still fetch the pinned record directly, independent of other sessions.
        assert!(state.native_session_home.is_none());
        assert!(source(&state, &native.id).is_err());
        let mut session = ClientSession::local(Some("person/example")).unwrap();
        session.conversation_blocks = true;
        let page = read(&native, &session, &native.id).unwrap();
        let token = page.last().unwrap()["body"]["blocks"][0]["continuation"]["ref"]
            .as_str()
            .unwrap();
        let encrypted = URL_SAFE_NO_PAD
            .decode(token.strip_prefix("v2.").unwrap())
            .unwrap();
        assert!(
            !encrypted
                .windows(native.transcript.as_os_str().len())
                .any(|bytes| bytes == native.transcript.to_string_lossy().as_bytes())
        );
        let location = locator(token, &native.id).unwrap();
        assert_eq!(location.source.transcript, native.transcript);
        let response = chunk_local(&state, &session, &native.id, token, 0)
            .await
            .unwrap();
        let decoded = STANDARD.decode(response["data"].as_str().unwrap()).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&decoded).unwrap(),
            json!({"raw":raw})
        );
        let mut forged = encrypted;
        *forged.last_mut().unwrap() ^= 1;
        assert!(
            locator(
                &format!("v2.{}", URL_SAFE_NO_PAD.encode(forged)),
                &native.id
            )
            .is_err()
        );
        assert!(
            locator(
                &format!("v1.{}", token.strip_prefix("v2.").unwrap()),
                &native.id
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn raw_text_continuations_recover_exact_non_utf8_bytes_and_invalidate_on_edit() {
        let root = tempfile::tempdir().unwrap();
        let native = fixture(root.path(), json!([]));
        let mut bytes = b"not JSON ".repeat(2000);
        bytes.extend_from_slice(b"\xff\x00unfinished");
        std::fs::write(&native.transcript, &bytes).unwrap();
        let state = super::super::tests::test_state_named(root.path(), "raw-bytes-test");
        let mut session = ClientSession::local(Some("person/example")).unwrap();
        session.conversation_blocks = true;
        let page = read(&native, &session, &native.id).unwrap();
        let block = &page[0]["body"]["blocks"][0];
        assert_eq!(block["kind"], "raw_text");
        assert!(block["payload"].as_str().unwrap().contains("truncated"));
        let token = block["continuation"]["ref"].as_str().unwrap();
        let mut offset = 0;
        let mut encoded = Vec::new();
        loop {
            let chunk = chunk_local(&state, &session, &native.id, token, offset)
                .await
                .unwrap();
            encoded.extend(STANDARD.decode(chunk["data"].as_str().unwrap()).unwrap());
            let Some(next) = chunk["next_offset"].as_u64() else {
                break;
            };
            offset = next;
        }
        let payload: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            STANDARD.decode(payload["bytes"].as_str().unwrap()).unwrap(),
            bytes
        );
        std::fs::write(&native.transcript, b"replacement").unwrap();
        assert_eq!(
            chunk_local(&state, &session, &native.id, token, 0)
                .await
                .unwrap_err()
                .status,
            StatusCode::GONE
        );
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
    #[tokio::test]
    async fn images_do_not_grant_file_or_network_authority_or_trust_native_mime() {
        let root = tempfile::tempdir().unwrap();
        let source = fixture(root.path(), json!([]));
        let blobs = root.path().join(".omp/agent/blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        let png = b"\x89PNG\r\n\x1a\npassive-invented-test";
        let digest = hex::encode(Sha256::digest(png));
        let path = blobs.join(&digest);
        std::fs::write(&path, png).unwrap();
        for data in [
            format!("blob:sha256:{digest}"),
            format!("file://{}", path.display()),
            format!("data:text/html;base64,{}", STANDARD.encode(png)),
        ] {
            let (bytes, media) = image_bytes(
                &source,
                &json!({"type":"image","mimeType":"text/html","data":data}),
            )
            .unwrap();
            assert_eq!(bytes, png);
            assert_eq!(media, "image/png");
        }
        let secret = root.path().join("invented-key");
        std::fs::write(&secret, "invented-private-key").unwrap();
        assert!(
            image_bytes(
                &source,
                &json!({"type":"image","data":format!("file://{}",secret.display())})
            )
            .is_err()
        );
        #[cfg(unix)]
        {
            let escape = blobs.join("escape");
            std::os::unix::fs::symlink(&secret, &escape).unwrap();
            assert!(
                image_bytes(
                    &source,
                    &json!({"type":"image","data":format!("file://{}",escape.display())})
                )
                .is_err()
            );
        }
        for bytes in [
            b"<svg><script>alert(1)</script></svg>".as_slice(),
            b"<html>active</html>",
            b"invented-private-key",
        ] {
            let (_, mime) = image_bytes(
                &source,
                &json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(bytes)}),
            )
            .unwrap();
            assert_eq!(mime, "application/octet-stream");
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        for url in [
            format!(
                "http://{}/redirect-to-private",
                listener.local_addr().unwrap()
            ),
            "http://169.254.169.254/latest/meta-data/".into(),
            "https://example.invalid/image".into(),
        ] {
            let value = json!({"type":"image","url":url});
            assert!(external_image(&value));
            assert!(image_bytes(&source, &value).is_err());
        }
        let linked = fixture(
            root.path(),
            json!([{"type":"image_url","image_url":{"url":"http://127.0.0.1/invented-image"}}]),
        );
        let mut session = ClientSession::local(Some("person/example")).unwrap();
        session.conversation_blocks = true;
        let page = read(&linked, &session, &linked.id).unwrap();
        let link = page
            .iter()
            .flat_map(|item| item["body"]["blocks"].as_array().into_iter().flatten())
            .find(|block| block["kind"] == "image_link")
            .unwrap();
        assert_eq!(
            link["payload"]["image_url"]["url"],
            "http://127.0.0.1/invented-image"
        );
        assert!(link.get("continuation").is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err(),
            "owner must not make even the first HTTP request"
        );
    }

    #[tokio::test]
    async fn paired_display_key_uses_projection_scope_for_raw_content_and_revocation() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;
        let root = tempfile::tempdir().unwrap();
        let native = fixture(
            root.path(),
            json!([{ "type":"future", "secret":"invented-token", "large":"x".repeat(10000) }]),
        );
        let mut state = super::super::tests::test_state_named(root.path(), "paired-owner-test");
        state.native_session_home = Some(root.path().into());
        let mut local = ClientSession::local(Some("person/example")).unwrap();
        local.conversation_blocks = true;
        let entries = read(&native, &local, &native.id).unwrap();
        let token = entries
            .iter()
            .flat_map(|item| item["body"]["blocks"].as_array().into_iter().flatten())
            .find_map(|block| block["continuation"]["ref"].as_str())
            .unwrap();
        let route = format!(
            "/v1/client/conversations/{}/content/{token}/chunk",
            native.id.trim_start_matches("session/")
        );
        let credential = "invented-display-credential";
        let app = super::super::super::fabric_router(state.clone());
        for (scopes, expected) in [
            (json!([]), StatusCode::FORBIDDEN),
            (json!(["read.projections"]), StatusCode::OK),
        ] {
            state
                .store
                .append_claim(&ClaimInput {
                    subject: "custom/client/invented-display".into(),
                    kind: "custom.client.pairing-completed".into(),
                    actor: Some("person/example".into()),
                    fields: BTreeMap::from([
                        (
                            "credential_hash".into(),
                            json!(credential_digest(credential)),
                        ),
                        ("session_actor".into(), json!("client/invented-display")),
                        ("person_id".into(), json!("person/example")),
                        ("scopes".into(), scopes),
                        (
                            "expires_at_unix_ms".into(),
                            json!(client_now_ms() as u64 + 60000),
                        ),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&route)
                        .header(AUTHORIZATION, format!("Bearer {credential}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if expected == StatusCode::OK {
                let bytes = axum::body::to_bytes(response.into_body(), CLIENT_MAX_RESPONSE_BYTES)
                    .await
                    .unwrap();
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                let raw = STANDARD
                    .decode(value["value"]["data"].as_str().unwrap())
                    .unwrap();
                assert!(String::from_utf8(raw).unwrap().contains("invented-token"));
            }
        }
        state
            .store
            .append_claim(&ClaimInput {
                subject: "custom/client/invented-display".into(),
                kind: "custom.client.pairing-revoked".into(),
                actor: Some("person/example".into()),
                fields: BTreeMap::new(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(route)
                    .header(AUTHORIZATION, format!("Bearer {credential}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn concurrent_read_limit_has_no_waiting_backlog() {
        let slots = tokio::sync::Semaphore::new(4);
        let held: Vec<_> = (0..4).map(|_| slots.try_acquire().unwrap()).collect();
        assert_eq!(read_slot(&slots).unwrap_err().code, "rate-limited");
        drop(held);
        assert!(read_slot(&slots).is_ok());
    }
    #[test]
    fn sqlite_ref_fences_native_message_and_part_identity_not_only_rowid() {
        use rusqlite::{Connection, params};
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("opencode.db");
        let db = Connection::open(&database).unwrap();
        db.execute_batch("CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,data TEXT); INSERT INTO message VALUES('message-one','native-test',1,'{\"role\":\"assistant\"}');").unwrap();
        let raw = json!({"type":"future","content":"invented".repeat(2000)});
        db.execute(
            "INSERT INTO part VALUES('part-one','native-test','message-one',1,?1)",
            params![raw.to_string()],
        )
        .unwrap();
        let mut native = fixture(root.path(), json!([]));
        native.driver = ExternalDriver::OpenCode;
        native.transcript = database;
        let mut session = ClientSession::local(Some("person/example")).unwrap();
        session.conversation_blocks = true;
        let page = read(&native, &session, &native.id).unwrap();
        let block = page
            .iter()
            .flat_map(|item| item["body"]["blocks"].as_array().into_iter().flatten())
            .find(|block| block["kind"] == "unknown")
            .unwrap();
        let token = block["continuation"]["ref"].as_str().unwrap();
        let location = locator(token, &native.id).unwrap();
        assert_eq!(located_value(&native, &location).unwrap()["raw"], raw);
        db.execute("UPDATE part SET id='renamed-part'", []).unwrap();
        assert!(located_value(&native, &location).is_err());
        db.execute("UPDATE part SET id='part-one'", []).unwrap();
        assert!(located_value(&native, &location).is_ok());
        db.execute("UPDATE message SET id='renamed-message'", [])
            .unwrap();
        db.execute("UPDATE part SET message_id='renamed-message'", [])
            .unwrap();
        assert!(located_value(&native, &location).is_err());
    }

    #[test]
    fn sqlite_large_rows_get_continuations_and_refs_survive_wal_appends() {
        use rusqlite::{Connection, params};
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("opencode.db");
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT); CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, data TEXT);").unwrap();
        connection
            .execute(
                "INSERT INTO message VALUES ('message-one','native-test',1,?1)",
                params![json!({"role":"assistant"}).to_string()],
            )
            .unwrap();
        let output = "x".repeat(17 * 1024 * 1024);
        let tool = json!({"type":"tool","callID":"call-one","tool":"shell","state":{"status":"completed","input":{"command":"invented"},"output":output}});
        connection
            .execute(
                "INSERT INTO part VALUES ('part-one','native-test','message-one',1,?1)",
                params![tool.to_string()],
            )
            .unwrap();
        let mut native = fixture(root.path(), json!([]));
        native.driver = ExternalDriver::OpenCode;
        native.transcript = database;
        let mut session = ClientSession::local(Some("person/example")).unwrap();
        session.conversation_blocks = true;
        let page = read(&native, &session, &native.id).unwrap();
        let item = page
            .iter()
            .find(|item| item["type"] == "tool_result")
            .expect("oversized native result is retained");
        assert_eq!(
            item["body"]["blocks"][0]["payload"],
            json!({"body_ref":true})
        );
        assert!(
            item["body"]["content"]
                .as_str()
                .unwrap()
                .contains("truncated")
        );
        let reference = item["body"]["blocks"][0]["continuation"]["ref"]
            .as_str()
            .unwrap();
        let location = locator(reference, &native.id).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
        let bound = basis(&native).unwrap();
        connection.execute("INSERT INTO part VALUES ('later','native-test','message-one',2,'{\"type\":\"text\",\"text\":\"later\"}')",[]).unwrap();
        connection
            .execute(
                "UPDATE message SET data = ?1 WHERE id = 'message-one'",
                params![
                    json!({"role":"assistant","tokens":{"output":42},"completed":true}).to_string()
                ],
            )
            .unwrap();
        assert_eq!(basis(&native).unwrap(), bound);
        assert_eq!(
            located_value(&native, &location).unwrap()["content"],
            output
        );
        let raw = json!({"type":"future","large":output});
        connection
            .execute(
                "INSERT INTO part VALUES ('oversized-unknown','native-test','message-one',3,?1)",
                params![raw.to_string()],
            )
            .unwrap();
        let page = read(&native, &session, &native.id).unwrap();
        let block = page
            .iter()
            .flat_map(|item| item["body"]["blocks"].as_array().into_iter().flatten())
            .find(|block| block["kind"] == "unknown")
            .expect("oversized raw item gets a display stub, not dropped");
        let token = block["continuation"]["ref"].as_str().unwrap();
        let full = located_value(&native, &locator(token, &native.id).unwrap()).unwrap();
        assert_eq!(full, json!({"raw":raw}));
        assert_eq!(
            block["continuation"]["size"],
            serde_json::to_vec(&full).unwrap().len()
        );
        assert!(serde_json::to_vec(&page).unwrap().len() < CLIENT_MAX_RESPONSE_BYTES);
        connection.execute("UPDATE part SET data = '{\"type\":\"text\",\"text\":\"edited\"}' WHERE id = 'part-one'",[]).unwrap();
        assert!(located_value(&native, &location).is_err());
        connection
            .execute(
                "UPDATE message SET data = ?1 WHERE id = 'message-one'",
                params![json!({"role":"future-role-token"}).to_string()],
            )
            .unwrap();
        let page = read(&native, &session, &native.id).unwrap();
        assert!(page.iter().all(|item| item["role"] == "system"));
        let block = page
            .iter()
            .flat_map(|item| item["body"]["blocks"].as_array().into_iter().flatten())
            .find(|block| block["continuation"].get("ref").is_some())
            .unwrap();
        let token = block["continuation"]["ref"].as_str().unwrap();
        let location = locator(token, &native.id).unwrap();
        assert_eq!(location.native["role"], "system");
        assert!(!location.native.to_string().contains("future-role-token"));
        assert_eq!(
            located_value(&native, &location).unwrap()["source_role"],
            "future-role-token"
        );
    }
}
