//! Paired-device derived credentials and the Expo Updates v1 read boundary.
//! Publishing is registered only on the owner-only Unix socket.
use super::*;
use axum::http::{HeaderMap, HeaderValue};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use client_v0::{ClientSession, require_scope, revalidate_session, validation};
use parking_lot::Mutex;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::LazyLock;

pub(super) const MAX_REQUEST_BYTES: usize = 180 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_ASSET_BYTES: usize = 32 * 1024 * 1024;
const MAX_EXPORT_BYTES: usize = 128 * 1024 * 1024;
const MAX_STORE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_PUBLICATIONS: usize = 10_000;
const TOKEN_TTL_MS: u128 = 15 * 60 * 1000;
const PUBLICATION_KIND: &str = "custom.app-update.published";
static TOKENS: LazyLock<Mutex<BTreeMap<(PathBuf, String), Token>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
static PUBLISH_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
struct Token {
    session: ClientSession,
    app: String,
    channel: String,
    expires_at: u128,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Target {
    app: String,
    channel: String,
}

fn tokens() -> &'static Mutex<BTreeMap<(PathBuf, String), Token>> {
    &TOKENS
}

fn forbidden(message: &str) -> ApiError {
    ApiError {
        status: StatusCode::FORBIDDEN,
        code: "forbidden".into(),
        message: message.into(),
        details: Box::default(),
    }
}

fn validate_target(target: &Target) -> Result<(), ApiError> {
    for value in [&target.app, &target.channel] {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return Err(validation("app and channel must be bounded identifiers"));
        }
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(super) fn is_protocol_read(path: &str) -> bool {
    path == "/v1/client/app-updates/manifest" || path.starts_with("/v1/client/app-updates/assets/")
}

pub(super) async fn mint(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Json(target): Json<Target>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.app-updates")?;
    if session.pairing_grant.is_none() {
        return Err(forbidden("an update token requires a paired credential"));
    }
    validate_target(&target)?;
    let current = revalidate_session(&state, &session)?;
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(ApiError::internal)?;
    let token = URL_SAFE_NO_PAD.encode(random);
    let now = client_now_ms();
    let expires_at = now + TOKEN_TTL_MS;
    let mut table = tokens().lock();
    table.retain(|_, token| token.expires_at > now);
    if table.len() >= 4096 {
        return Err(validation("the update token capacity is exhausted"));
    }
    table.insert(
        (state.state_dir, digest(token.as_bytes())),
        Token {
            session: current,
            app: target.app,
            channel: target.channel,
            expires_at,
        },
    );
    Ok(Json(json!({"token":token,"expiresAtUnixMs":expires_at})))
}

/// A paired credential revokes all outstanding derived tokens from that exact grant.
pub(super) async fn revoke(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
) -> Result<Json<Value>, ApiError> {
    require_scope(&session, "read.app-updates")?;
    if session.pairing_grant.is_none() {
        return Err(forbidden("revocation requires a paired credential"));
    }
    tokens().lock().retain(|(root, _), token| {
        root != &state.state_dir || token.session.pairing_grant != session.pairing_grant
    });
    Ok(Json(json!({"revoked":true})))
}

pub(super) fn authenticate(
    state: &AppState,
    request: &Request<Body>,
    bearer: &str,
    transport: &'static str,
) -> Result<ClientSession, ApiError> {
    let token = tokens()
        .lock()
        .get(&(state.state_dir.clone(), digest(bearer.as_bytes())))
        .cloned()
        .ok_or_else(|| forbidden("an update-only token is required"))?;
    if token.expires_at <= client_now_ms() {
        return Err(forbidden("the update token expired"));
    }
    let url = reqwest::Url::parse(&format!("http://localhost{}", request.uri()))
        .map_err(|_| validation("invalid update target"))?;
    let pairs = url.query_pairs().collect::<Vec<_>>();
    for (name, value) in [("app", &token.app), ("channel", &token.channel)] {
        let actual = pairs
            .iter()
            .filter(|(key, _)| key == name)
            .collect::<Vec<_>>();
        if actual.len() != 1 || actual[0].1 != value.as_str() {
            return Err(forbidden("the token cannot read this app or channel"));
        }
    }
    let mut session = revalidate_session(state, &token.session)?;
    require_scope(&session, "read.app-updates")?;
    session.transport = transport;
    Ok(session)
}

fn subject(state: &AppState, target: &Target) -> String {
    format!(
        "custom/app-updates/{}",
        digest(format!("{}\0{}\0{}", state.node, target.app, target.channel).as_bytes())
    )
}

fn publications(state: &AppState, target: &Target) -> Result<Vec<ClaimRecord>, ApiError> {
    Ok(state
        .store
        .claims_for_subject_kind_at(
            &subject(state, target),
            PUBLICATION_KIND,
            None,
            true,
            MAX_PUBLICATIONS,
        )
        .map_err(ApiError::internal)?
        .claims)
}

fn field<'a>(record: &'a ClaimRecord, key: &str) -> &'a Value {
    &record.body["fields"][key]
}
fn string_field<'a>(record: &'a ClaimRecord, key: &str) -> Result<&'a str, ApiError> {
    field(record, key)
        .as_str()
        .ok_or_else(|| ApiError::internal("invalid stored app-update metadata"))
}

fn objects(state: &AppState) -> PathBuf {
    state.state_dir.join("app-updates-v1")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Publish {
    app: String,
    channel: String,
    manifest: String,
    signature: String,
    assets: Vec<ImportAsset>,
    source_ref: Option<String>,
    expected_head: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportAsset {
    hash: String,
    content_type: String,
    bytes: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    id: String,
    created_at: String,
    runtime_version: String,
    launch_asset: ManifestAsset,
    assets: Vec<ManifestAsset>,
    metadata: BTreeMap<String, String>,
    extra: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestAsset {
    hash: String,
    key: String,
    content_type: String,
    url: String,
    file_extension: Option<String>,
}

fn decode(value: &str, limit: usize) -> Result<Vec<u8>, ApiError> {
    if value.len() > limit.div_ceil(3) * 4 {
        return Err(validation("the update object exceeds its byte limit"));
    }
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| validation("update objects require base64 encoding"))?;
    if bytes.len() > limit {
        return Err(validation("the update object exceeds its byte limit"));
    }
    Ok(bytes)
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn store_object(root: &Path, hash: &str, bytes: &[u8]) -> Result<(), ApiError> {
    let path = root.join(hash);
    if path.exists() {
        if digest(&fs::read(path).map_err(ApiError::internal)?) != hash {
            return Err(ApiError::internal(
                "a stored immutable update object is corrupt",
            ));
        }
        return Ok(());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(root).map_err(ApiError::internal)?;
    temporary.write_all(bytes).map_err(ApiError::internal)?;
    temporary.as_file().sync_all().map_err(ApiError::internal)?;
    temporary
        .persist_noclobber(path)
        .map_err(ApiError::internal)?;
    Ok(())
}

pub(super) async fn publish(
    State(state): State<AppState>,
    Json(input): Json<Publish>,
) -> Result<Json<Value>, ApiError> {
    let target = Target {
        app: input.app,
        channel: input.channel,
    };
    validate_target(&target)?;
    let bytes = decode(&input.manifest, MAX_MANIFEST_BYTES)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| validation("invalid Expo manifest"))?;
    uuid::Uuid::parse_str(&manifest.id)
        .map_err(|_| validation("the manifest id must be a UUID"))?;
    let created_at = chrono::DateTime::parse_from_rfc3339(&manifest.created_at)
        .map_err(|_| validation("invalid manifest creation time"))?;
    if manifest.runtime_version.is_empty()
        || manifest.runtime_version.len() > 256
        || manifest.runtime_version.chars().any(char::is_control)
        || !manifest.extra.is_object()
    {
        return Err(validation("invalid manifest compatibility metadata"));
    }
    if input.assets.is_empty() || input.assets.len() > 512 || manifest.assets.len() > 511 {
        return Err(validation("the export asset count exceeds its limit"));
    }
    if input
        .source_ref
        .as_ref()
        .is_some_and(|value| value.len() > 256 || value.chars().any(char::is_control))
    {
        return Err(validation("invalid source ref"));
    }
    // Private signing material never enters this service. The device verifies the signature.
    validate_signature(&input.signature)?;
    let mut imported = BTreeMap::new();
    let mut total = 0_usize;
    for asset in input.assets {
        if !valid_hash(&asset.hash)
            || HeaderValue::from_str(&asset.content_type).is_err()
            || asset.content_type.len() > 128
            || !asset.content_type.contains('/')
        {
            return Err(validation("invalid asset metadata"));
        }
        let content = decode(&asset.bytes, MAX_ASSET_BYTES)?;
        total += content.len();
        if total > MAX_EXPORT_BYTES {
            return Err(validation("the export exceeds its aggregate byte limit"));
        }
        if digest(&content) != asset.hash {
            return Err(validation("an exported asset failed SHA-256 verification"));
        }
        if imported
            .insert(asset.hash, (asset.content_type, content))
            .is_some()
        {
            return Err(validation("duplicate exported asset"));
        }
    }
    let mut referenced = BTreeMap::new();
    let mut keys = BTreeSet::new();
    let mut origin = None;
    for asset in std::iter::once(&manifest.launch_asset).chain(&manifest.assets) {
        let hash_bytes = URL_SAFE_NO_PAD
            .decode(&asset.hash)
            .map_err(|_| validation("asset hashes must be base64url SHA-256"))?;
        if hash_bytes.len() != 32
            || asset.key.is_empty()
            || !keys.insert(&asset.key)
            || asset.file_extension.as_ref().is_some_and(|extension| {
                !extension.starts_with('.') || extension.contains('/') || extension.len() > 32
            })
        {
            return Err(validation("invalid manifest asset identity"));
        }
        let hash = hex::encode(hash_bytes);
        let Some((content_type, _)) = imported.get(&hash) else {
            return Err(validation("a manifest asset is missing from the export"));
        };
        if content_type != &asset.content_type {
            return Err(validation("asset MIME types disagree"));
        }
        let url = reqwest::Url::parse(&asset.url)
            .map_err(|_| validation("asset URLs must be absolute"))?;
        if !matches!(url.scheme(), "https" | "http")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.path() != format!("/v1/client/app-updates/assets/{hash}")
        {
            return Err(validation(
                "asset URLs must name authenticated content-addressed gateway assets",
            ));
        }
        if url.query_pairs().count() != 2 {
            return Err(validation(
                "asset URL target requires exactly one app and channel",
            ));
        }
        let pairs = url.query_pairs().collect::<BTreeMap<_, _>>();
        if pairs.len() != 2
            || pairs.get("app").map(|s| s.as_ref()) != Some(target.app.as_str())
            || pairs.get("channel").map(|s| s.as_ref()) != Some(target.channel.as_str())
        {
            return Err(validation("asset URL target does not match publication"));
        }
        let asset_origin = url.origin().ascii_serialization();
        if origin
            .as_ref()
            .is_some_and(|origin| origin != &asset_origin)
        {
            return Err(validation("all assets must share the gateway origin"));
        }
        origin = Some(asset_origin);
        referenced.insert(hash, asset.content_type.clone());
    }
    if referenced.len() != imported.len() {
        return Err(validation("the export contains unreferenced assets"));
    }
    let _serialized = PUBLISH_LOCK.lock();
    let previous = publications(&state, &target)?;
    let head = previous
        .first()
        .map(|record| string_field(record, "id"))
        .transpose()?;
    if input
        .expected_head
        .as_deref()
        .is_some_and(|expected| head != Some(expected))
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "stale-update-head".into(),
            message: "the channel head changed while exporting".into(),
            details: Box::default(),
        });
    }
    let manifest_hash = digest(&bytes);
    if let Some(existing) = previous
        .iter()
        .find(|record| field(record, "id").as_str() == Some(&manifest.id))
    {
        if string_field(existing, "manifest_hash")? != manifest_hash
            || string_field(existing, "signature")? != input.signature
        {
            return Err(validation(
                "an update UUID cannot be reused for different signed bytes",
            ));
        }
        return Ok(Json(
            json!({"id":manifest.id,"app":target.app,"channel":target.channel,"runtimeVersion":manifest.runtime_version}),
        ));
    }
    if previous.len() >= MAX_PUBLICATIONS {
        return Err(validation("the retained publication capacity is exhausted"));
    }
    if let Some(current) = previous
        .iter()
        .find(|record| field(record, "runtime_version").as_str() == Some(&manifest.runtime_version))
    {
        let current_time =
            chrono::DateTime::parse_from_rfc3339(string_field(current, "created_at")?)
                .map_err(ApiError::internal)?;
        if created_at <= current_time {
            return Err(validation(
                "stale exports cannot replace a newer compatible publication",
            ));
        }
    }
    let root = objects(&state);
    fs::create_dir_all(&root).map_err(ApiError::internal)?;
    fs::File::open(&state.state_dir)
        .and_then(|dir| dir.sync_all())
        .map_err(ApiError::internal)?;
    let stored_bytes = fs::read_dir(&root)
        .map_err(ApiError::internal)?
        .try_fold(0_u64, |sum, entry| -> std::io::Result<u64> {
            Ok(sum + entry?.metadata()?.len())
        })
        .map_err(ApiError::internal)?;
    let added_bytes: u64 = imported
        .iter()
        .filter(|(hash, _)| !root.join(hash).exists())
        .map(|(_, (_, bytes))| bytes.len() as u64)
        .sum::<u64>()
        + if root.join(&manifest_hash).exists() {
            0
        } else {
            bytes.len() as u64
        };
    if stored_bytes.saturating_add(added_bytes) > MAX_STORE_BYTES {
        return Err(validation(
            "the immutable app-update store is full; referenced assets are never evicted",
        ));
    }
    for (hash, (_, content)) in &imported {
        store_object(&root, hash, content)?;
    }
    store_object(&root, &manifest_hash, &bytes)?;
    fs::File::open(&root)
        .and_then(|dir| dir.sync_all())
        .map_err(ApiError::internal)?;
    // The durable claim is the only head promotion, after every object and directory is synced.
    state.store.append_claim(&ClaimInput {
        subject: subject(&state, &target), kind: PUBLICATION_KIND.into(), actor: None,
        fields: serde_json::from_value(json!({"id":manifest.id,"app":target.app,"channel":target.channel,"node":state.node,"runtime_version":manifest.runtime_version,"created_at":manifest.created_at,"manifest_hash":manifest_hash,"signature":input.signature,"origin":origin,"assets":referenced,"source_ref":input.source_ref,"metadata":manifest.metadata})).map_err(ApiError::internal)?,
        evidence: vec![], expected_subject: None, idempotency_key: None,
    }).map_err(ApiError::bad)?;
    state.notify.notify_one();
    Ok(Json(
        json!({"id":manifest.id,"app":target.app,"channel":target.channel,"runtimeVersion":manifest.runtime_version}),
    ))
}

fn validate_signature(signature: &str) -> Result<(), ApiError> {
    if signature.len() > 4096 || HeaderValue::from_str(signature).is_err() {
        return Err(validation("invalid Expo signature header"));
    }
    let fields = signature.split(',').map(str::trim).collect::<Vec<_>>();
    let sig = fields
        .iter()
        .find_map(|field| {
            field
                .strip_prefix("sig=\"")
                .and_then(|value| value.strip_suffix('"'))
        })
        .ok_or_else(|| validation("a signed manifest is required"))?;
    if !fields.contains(&"keyid=\"main\"")
        || fields
            .iter()
            .any(|field| field.starts_with("alg=") && *field != "alg=\"rsa-v1_5-sha256\"")
        || STANDARD.decode(sig).map_or(true, |bytes| bytes.len() < 256)
    {
        return Err(validation(
            "the Expo signing key must be main with RSA SHA-256",
        ));
    }
    Ok(())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn negotiate(headers: &HeaderMap) -> Result<&'static str, ApiError> {
    let accept = header(headers, "accept").unwrap_or("*/*");
    let mut best = None;
    for (priority, content_type) in [
        "multipart/mixed",
        "application/expo+json",
        "application/json",
    ]
    .into_iter()
    .enumerate()
    {
        let mut chosen = None;
        for entry in accept.split(',') {
            let mut parameters = entry.trim().split(';');
            let media = parameters.next().unwrap_or_default().trim();
            let specificity = if media == content_type {
                2
            } else if media == "*/*" {
                0
            } else if media == "application/*" && content_type.starts_with("application/") {
                1
            } else {
                continue;
            };
            let quality = parameters
                .find_map(|parameter| parameter.trim().strip_prefix("q="))
                .unwrap_or("1")
                .parse::<f32>()
                .unwrap_or(0.0);
            if chosen.is_none_or(|(current, _)| specificity > current) {
                chosen = Some((specificity, quality));
            }
        }
        if let Some((_, quality)) = chosen.filter(|(_, q)| q.is_finite() && *q > 0.0 && *q <= 1.0) {
            if best.is_none_or(|(q, p, _)| quality > q || quality == q && priority < p) {
                best = Some((quality, priority, content_type));
            }
        }
    }
    best.map(|(_, _, media)| media).ok_or_else(|| ApiError {
        status: StatusCode::NOT_ACCEPTABLE,
        code: "unsupported-update-response".into(),
        message: "the client must accept an Expo manifest or multipart response".into(),
        details: Box::default(),
    })
}

fn common_headers(response: &mut Response) {
    for (name, value) in [
        ("expo-protocol-version", "1"),
        ("expo-sfv-version", "0"),
        ("expo-manifest-filters", ""),
        ("expo-server-defined-headers", ""),
        ("cache-control", "private, max-age=0, no-store"),
        (
            "vary",
            "Authorization, Accept, Expo-Platform, Expo-Runtime-Version",
        ),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
}

pub(super) async fn manifest(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(target): Query<Target>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_scope(&session, "read.app-updates")?;
    validate_target(&target)?;
    if header(&headers, "expo-protocol-version") != Some("1") {
        return Err(ApiError {
            status: StatusCode::NOT_ACCEPTABLE,
            code: "unsupported-update-protocol".into(),
            message: "only Expo Updates protocol version 1 is supported".into(),
            details: Box::default(),
        });
    }
    if header(&headers, "expo-platform") != Some("ios") {
        return Err(ApiError::not_found(
            "this app-update store serves the iOS platform",
        ));
    }
    let runtime = header(&headers, "expo-runtime-version")
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| validation("expo-runtime-version is required"))?;
    if header(&headers, "expo-expect-signature")
        .is_some_and(|value| value.contains("keyid=") && !value.contains("keyid=\"main\""))
    {
        return Err(validation("the requested signing key is unavailable"));
    }
    let media = negotiate(&headers)?;
    let records = publications(&state, &target)?;
    if let Some(matching) = records
        .iter()
        .find(|record| field(record, "runtime_version").as_str() == Some(runtime))
    {
        let origin = string_field(matching, "origin")?;
        let authority = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"));
        if authority != header(&headers, "host") {
            return Err(validation(
                "the signed manifest belongs to another gateway origin; re-export and sign for this gateway",
            ));
        }
    }
    if let Some(latest) = records.first() {
        if string_field(latest, "runtime_version")? != runtime {
            state.store.append_claim(&ClaimInput {
                subject: format!("daemon/{}", state.node), kind: "app-update.native-build-required".into(), actor: None,
                fields: serde_json::from_value(json!({"app":target.app,"channel":target.channel,"pairing_grant":session.pairing_grant,"installed_runtime":runtime,"required_runtime":string_field(latest,"runtime_version")?,"update_id":string_field(latest,"id")?})).map_err(ApiError::internal)?,
                evidence: vec![], expected_subject: None,
                idempotency_key: Some(digest(format!("{}\0{}\0{}",session.pairing_grant.as_deref().unwrap_or_default(),string_field(latest,"id")?,runtime).as_bytes())),
            }).map_err(ApiError::bad)?;
        }
    }
    let matching = records
        .iter()
        .find(|record| field(record, "runtime_version").as_str() == Some(runtime));
    let matching = matching.filter(|record| {
        header(&headers, "expo-current-update-id") != field(record, "id").as_str()
    });
    let Some(matching) = matching else {
        if media != "multipart/mixed" {
            return Err(ApiError {
                status: StatusCode::NOT_ACCEPTABLE,
                code: "no-update".into(),
                message: "no update is available; accept multipart/mixed for an empty response"
                    .into(),
                details: Box::default(),
            });
        }
        let mut response = StatusCode::NO_CONTENT.into_response();
        common_headers(&mut response);
        return Ok(response);
    };
    let bytes = fs::read(objects(&state).join(string_field(matching, "manifest_hash")?))
        .map_err(ApiError::internal)?;
    let signature = string_field(matching, "signature")?;
    let mut response = if media == "multipart/mixed" {
        let boundary = format!("st-update-{}", string_field(matching, "manifest_hash")?);
        let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"manifest\"\r\nContent-Type: application/expo+json\r\nexpo-signature: {signature}\r\n\r\n").into_bytes();
        body.extend_from_slice(&bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let mut response = body.into_response();
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_str(&format!("multipart/mixed; boundary={boundary}"))
                .map_err(ApiError::internal)?,
        );
        response
    } else {
        let mut response = bytes.into_response();
        response
            .headers_mut()
            .insert("content-type", HeaderValue::from_static(media));
        response.headers_mut().insert(
            "expo-signature",
            HeaderValue::from_str(signature).map_err(ApiError::internal)?,
        );
        response
    };
    common_headers(&mut response);
    Ok(response)
}

pub(super) async fn asset(
    State(state): State<AppState>,
    Extension(session): Extension<ClientSession>,
    Query(target): Query<Target>,
    AxumPath(hash): AxumPath<String>,
) -> Result<Response, ApiError> {
    require_scope(&session, "read.app-updates")?;
    validate_target(&target)?;
    if !valid_hash(&hash) {
        return Err(validation("invalid asset SHA-256"));
    }
    let records = publications(&state, &target)?;
    let content_type = records
        .iter()
        .find_map(|record| field(record, "assets").get(&hash).and_then(Value::as_str))
        .ok_or_else(|| forbidden("the asset is not referenced by this app and channel"))?;
    let bytes = fs::read(objects(&state).join(&hash)).map_err(ApiError::internal)?;
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_str(content_type).map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("private, max-age=0, no-store"),
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            app: "com.compoundingtech.smalltalk".into(),
            channel: "daily".into(),
        }
    }

    fn pairing(state: &AppState, scopes: Value) -> ClientSession {
        let paired = state.store.append_claim(&ClaimInput {
            subject: "custom/client/phone".into(), kind: "custom.client.pairing-completed".into(), actor: None,
            fields: serde_json::from_value(json!({"session_actor":"client/phone","person_id":"person/ada","credential_hash":digest(b"paired"),"expires_at_unix_ms":u64::MAX,"scopes":scopes})).unwrap(),
            evidence: vec![], expected_subject: None, idempotency_key: None,
        }).unwrap();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/client/app-updates/token")
            .header("authorization", "Bearer paired")
            .body(Body::empty())
            .unwrap();
        let session = client_v0::authenticate(state, &request, "fabric-loopback").unwrap();
        assert_eq!(
            session.pairing_grant.as_deref(),
            Some(paired.subject.as_str())
        );
        session
    }

    fn update_request(token: &str, app: &str, channel: &str) -> Request<Body> {
        Request::builder()
            .uri(format!(
                "/v1/client/app-updates/manifest?app={app}&channel={channel}"
            ))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn token_scope_expiry_and_revocation() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let denied = pairing(&state, json!([]));
        assert!(
            mint(State(state.clone()), Extension(denied), Json(target()))
                .await
                .is_err()
        );
        let session = pairing(&state, json!(["read.app-updates"]));
        let token = mint(
            State(state.clone()),
            Extension(session.clone()),
            Json(target()),
        )
        .await
        .unwrap()
        .0["token"]
            .as_str()
            .unwrap()
            .to_owned();
        let request = update_request(&token, &target().app, "daily");
        assert!(
            client_v0::authenticate(&state, &request, "fabric-loopback").is_ok(),
            "read.projections is not required"
        );
        assert!(
            client_v0::authenticate(
                &state,
                &update_request("paired", &target().app, "daily"),
                "fabric-loopback"
            )
            .is_err(),
            "broad pairing cannot read updates"
        );
        assert!(
            client_v0::authenticate(
                &state,
                &update_request(&token, "other", "daily"),
                "fabric-loopback"
            )
            .is_err()
        );
        assert!(
            client_v0::authenticate(
                &state,
                &update_request(&token, &target().app, "other"),
                "fabric-loopback"
            )
            .is_err()
        );
        let unrelated = Request::builder()
            .uri("/v1/client/agents")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert!(client_v0::authenticate(&state, &unrelated, "fabric-loopback").is_err());
        tokens()
            .lock()
            .get_mut(&(state.state_dir.clone(), digest(token.as_bytes())))
            .unwrap()
            .expires_at = client_now_ms();
        assert!(client_v0::authenticate(&state, &request, "fabric-loopback").is_err());
        let token = mint(
            State(state.clone()),
            Extension(session.clone()),
            Json(target()),
        )
        .await
        .unwrap()
        .0["token"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            revoke(State(state.clone()), Extension(session.clone()))
                .await
                .unwrap()
                .0["revoked"],
            true
        );
        assert!(
            client_v0::authenticate(
                &state,
                &update_request(&token, &target().app, "daily"),
                "fabric-loopback"
            )
            .is_err()
        );
        let token = mint(State(state.clone()), Extension(session), Json(target()))
            .await
            .unwrap()
            .0["token"]
            .as_str()
            .unwrap()
            .to_owned();
        state
            .store
            .append_claim(&ClaimInput {
                subject: "custom/client/phone".into(),
                kind: "custom.client.pairing-revoked".into(),
                actor: None,
                fields: BTreeMap::new(),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: None,
            })
            .unwrap();
        assert!(
            client_v0::authenticate(
                &state,
                &update_request(&token, &target().app, "daily"),
                "fabric-loopback"
            )
            .is_err()
        );
    }

    fn export(runtime: &str, timestamp: &str) -> Publish {
        let bytes = b"console.log('signed update')";
        let hash = digest(bytes);
        let manifest = json!({"id":uuid::Uuid::now_v7().to_string(),"createdAt":timestamp,"runtimeVersion":runtime,"launchAsset":{"hash":URL_SAFE_NO_PAD.encode(Sha256::digest(bytes)),"key":"bundler-md5","contentType":"application/javascript","url":format!("https://gateway.example/v1/client/app-updates/assets/{hash}?app={}&channel=daily",target().app)},"assets":[],"metadata":{},"extra":{}});
        Publish {
            app: target().app,
            channel: "daily".into(),
            manifest: STANDARD.encode(serde_json::to_vec(&manifest).unwrap()),
            signature: format!("sig=\"{}\", keyid=\"main\"", STANDARD.encode([1; 256])),
            assets: vec![ImportAsset {
                hash,
                content_type: "application/javascript".into(),
                bytes: STANDARD.encode(bytes),
            }],
            source_ref: Some("main".into()),
            expected_head: None,
        }
    }

    fn headers(runtime: &str, accept: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("expo-protocol-version", HeaderValue::from_static("1"));
        headers.insert("expo-platform", HeaderValue::from_static("ios"));
        headers.insert(
            "expo-runtime-version",
            HeaderValue::from_str(runtime).unwrap(),
        );
        headers.insert("accept", HeaderValue::from_str(accept).unwrap());
        headers.insert("host", HeaderValue::from_static("gateway.example"));
        headers
    }

    #[tokio::test]
    async fn compatible_manifest_signed_bytes_retention_and_deduplicated_mismatch() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let session = pairing(&state, json!(["read.app-updates"]));
        let mut wrong_protocol = headers("native-1", "multipart/mixed");
        wrong_protocol.insert("expo-protocol-version", HeaderValue::from_static("0"));
        assert_eq!(
            manifest(
                State(state.clone()),
                Extension(session.clone()),
                Query(target()),
                wrong_protocol
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::NOT_ACCEPTABLE
        );
        let mut wrong_platform = headers("native-1", "multipart/mixed");
        wrong_platform.insert("expo-platform", HeaderValue::from_static("android"));
        assert_eq!(
            manifest(
                State(state.clone()),
                Extension(session.clone()),
                Query(target()),
                wrong_platform
            )
            .await
            .unwrap_err()
            .status,
            StatusCode::NOT_FOUND
        );
        let first = export("native-1", "2026-10-07T01:00:00Z");
        let first_bytes = STANDARD.decode(&first.manifest).unwrap();
        let old_id = publish(State(state.clone()), Json(first)).await.unwrap().0["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = manifest(
            State(state.clone()),
            Extension(session.clone()),
            Query(target()),
            headers("native-1", "application/json"),
        )
        .await
        .unwrap();
        assert_eq!(response.headers()["expo-protocol-version"], "1");
        assert!(response.headers().contains_key("expo-signature"));
        assert_eq!(
            to_bytes(response.into_body(), MAX_MANIFEST_BYTES)
                .await
                .unwrap()
                .as_ref(),
            first_bytes
        );
        assert_eq!(
            publish(
                State(state.clone()),
                Json(export("native-2", "2026-10-07T02:00:00Z"))
            )
            .await
            .unwrap()
            .0["runtimeVersion"],
            "native-2"
        );
        for _ in 0..2 {
            let response = manifest(
                State(state.clone()),
                Extension(session.clone()),
                Query(target()),
                headers("native-1", "application/expo+json"),
            )
            .await
            .unwrap();
            let bytes = to_bytes(response.into_body(), MAX_MANIFEST_BYTES)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&bytes).unwrap()["id"],
                old_id
            );
        }
        let observations = state
            .store
            .observations_for("daemon/node", "app-update.native-build-required")
            .unwrap();
        assert_eq!(observations.len(), 1);
        assert!(
            state
                .store
                .claims_for("daemon/node", Some("app-update.native-build-required"))
                .unwrap()
                .is_empty(),
            "mismatch is node-local"
        );
        let mut current = headers("native-1", "multipart/mixed");
        current.insert(
            "expo-current-update-id",
            HeaderValue::from_str(&old_id).unwrap(),
        );
        assert_eq!(
            manifest(
                State(state.clone()),
                Extension(session.clone()),
                Query(target()),
                current
            )
            .await
            .unwrap()
            .status(),
            StatusCode::NO_CONTENT
        );
        let response = manifest(
            State(state.clone()),
            Extension(session.clone()),
            Query(target()),
            headers("native-1", "multipart/mixed"),
        )
        .await
        .unwrap();
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("multipart/mixed;")
        );
        let hash = digest(b"console.log('signed update')");
        assert!(
            asset(
                State(state.clone()),
                Extension(session.clone()),
                Query(target()),
                AxumPath(hash.clone())
            )
            .await
            .is_ok()
        );
        assert!(
            asset(
                State(state.clone()),
                Extension(session),
                Query(Target {
                    app: "other".into(),
                    channel: "daily".into()
                }),
                AxumPath(hash)
            )
            .await
            .is_err()
        );
        assert!(
            publish(
                State(state.clone()),
                Json(export("native-1", "2026-10-07T00:00:00Z"))
            )
            .await
            .is_err(),
            "stale job cannot displace newer runtime head"
        );
        let mut stale = export("native-1", "2026-10-07T03:00:00Z");
        stale.expected_head = Some(old_id);
        assert_eq!(
            publish(State(state), Json(stale)).await.unwrap_err().status,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn gateway_never_registers_publish_and_no_anonymous_reads() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        pairing(&state, json!(["read.app-updates", "control.pairing"]));
        let app = fabric_router(state.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/app-updates/publish")
                    .header("authorization", "Bearer paired")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/client/app-updates/publish")
                    .header("authorization", "Bearer paired")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        for app in [app, router(state)] {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/v1/client/app-updates/manifest?app={}&channel=daily",
                            target().app
                        ))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }

    #[test]
    fn manifest_negotiation_respects_quality_and_explicit_exclusion() {
        assert_eq!(
            negotiate(&headers(
                "n",
                "application/json;q=0.8, application/expo+json;q=0.9, multipart/mixed"
            ))
            .unwrap(),
            "multipart/mixed"
        );
        assert_eq!(
            negotiate(&headers("n", "application/json;q=0, */*;q=0.5")).unwrap(),
            "multipart/mixed"
        );
        assert!(negotiate(&headers("n", "application/json;q=0")).is_err());
        assert!(negotiate(&headers("n", "text/html")).is_err());
    }

    #[tokio::test]
    async fn failed_import_never_promotes_a_head() {
        let root = tempfile::tempdir().unwrap();
        let state = super::super::tests::state(root.path());
        let mut corrupt = export("n", "2026-10-07T01:00:00Z");
        corrupt.assets[0].bytes = STANDARD.encode(b"corrupt");
        assert!(publish(State(state.clone()), Json(corrupt)).await.is_err());
        assert!(publications(&state, &target()).unwrap().is_empty());
        assert!(!objects(&state).exists());
        let mut unsigned = export("n", "2026-10-07T01:00:00Z");
        unsigned.signature.clear();
        assert!(publish(State(state), Json(unsigned)).await.is_err());
    }
}
