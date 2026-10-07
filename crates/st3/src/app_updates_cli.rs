//! `st app-updates publish`: hand a signed Expo export to the local daemon.
//!
//! `apps/ios/scripts/sign-app-update.mjs` writes `manifest.json` (the exact signed bytes),
//! `manifest.signature` (the `expo-signature` header) and `publication.json` (each asset's
//! SHA-256, content type and file) into the export directory. This command checks that
//! directory against the manifest and the daemon's bounds before it sends anything, so a
//! broken export fails here with the file that is wrong.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use st3::client::{Client, Endpoint};

/// The daemon's publish bounds.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 32 * 1024 * 1024;
const MAX_TOTAL_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ASSETS: usize = 512;
const MAX_SIGNATURE_BYTES: u64 = 4096;
const MAX_INDEX_BYTES: u64 = 1024 * 1024;
const MAX_SOURCE_REF_CHARS: usize = 256;

pub const PUBLISH_PATH: &str = "/v1/app-updates/publish";

#[derive(clap::Subcommand)]
pub enum AppUpdatesCommand {
    /// Publish a signed Expo export as the newest update on one app channel.
    ///
    /// Sign the export first with apps/ios/scripts/sign-app-update.mjs. Publishing goes
    /// only through this machine's local socket; paired devices can read updates but never
    /// publish them.
    Publish(PublishArgs),
}

#[derive(clap::Args)]
pub struct PublishArgs {
    /// The app the update is for, as the signed export names it.
    #[arg(long)]
    pub app: String,
    /// The channel whose head becomes this update, for example `daily`.
    #[arg(long)]
    pub channel: String,
    /// The signed `expo export` directory.
    #[arg(long, value_name = "DIR")]
    pub dir: PathBuf,
    /// The branch or ref the export was built from, recorded with the publication. A manual
    /// branch publish stays the channel head only until the next publish, such as the next
    /// build of main.
    #[arg(long = "ref", value_name = "REF")]
    pub source_ref: Option<String>,
    /// Publish only while the channel head is still this update ID, so a stale job cannot
    /// replace a newer selection.
    #[arg(long, value_name = "UPDATE_ID", value_parser = parse_update_id)]
    pub expected_head: Option<String>,
}

fn parse_update_id(value: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(value)
        .map(|id| id.hyphenated().to_string())
        .map_err(|error| format!("not an update UUID: {error}"))
}

/// The local publish request body.
#[derive(Serialize)]
pub struct PublishRequest {
    pub app: String,
    pub channel: String,
    /// Base64 of the exact signed manifest bytes.
    pub manifest: String,
    pub signature: String,
    pub assets: Vec<PublishAsset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_head: Option<String>,
}

#[derive(Serialize)]
pub struct PublishAsset {
    /// Lowercase hex SHA-256 of the bytes.
    pub hash: String,
    pub content_type: String,
    /// Base64 of the asset bytes.
    pub bytes: String,
}

#[derive(Deserialize, Serialize)]
pub struct PublishResponse {
    pub id: String,
    pub app: String,
    pub channel: String,
    #[serde(rename = "runtimeVersion")]
    pub runtime_version: String,
}

/// `publication.json`, as the signing helper writes it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationIndex {
    app: String,
    channel: String,
    #[serde(rename = "runtimeVersion")]
    runtime_version: String,
    id: String,
    assets: Vec<IndexedAsset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IndexedAsset {
    hash: String,
    content_type: String,
    path: String,
}

#[derive(Deserialize)]
struct Manifest {
    id: String,
    #[serde(rename = "runtimeVersion")]
    runtime_version: String,
    #[serde(rename = "launchAsset")]
    launch_asset: ManifestAsset,
    assets: Vec<ManifestAsset>,
}

#[derive(Deserialize)]
struct ManifestAsset {
    hash: String,
    #[serde(rename = "contentType")]
    content_type: String,
    url: String,
}

pub async fn run(
    client: &Client,
    endpoint: &Endpoint,
    command: AppUpdatesCommand,
) -> Result<PublishResponse> {
    match command {
        AppUpdatesCommand::Publish(args) => {
            anyhow::ensure!(
                matches!(endpoint, Endpoint::Unix(_)),
                "st app-updates publish talks only to the local st socket; drop --endpoint or name a socket path"
            );
            let request = prepare(&args)?;
            client.post(PUBLISH_PATH, &request).await
        }
    }
}

/// Check the export directory and build the request without contacting the daemon.
pub fn prepare(args: &PublishArgs) -> Result<PublishRequest> {
    if let Some(source_ref) = &args.source_ref {
        anyhow::ensure!(
            !source_ref.is_empty()
                && source_ref.chars().count() <= MAX_SOURCE_REF_CHARS
                && !source_ref.chars().any(char::is_control),
            "--ref must be 1-{MAX_SOURCE_REF_CHARS} characters without control characters"
        );
    }
    let dir = args
        .dir
        .canonicalize()
        .with_context(|| format!("export directory {}", args.dir.display()))?;
    anyhow::ensure!(dir.is_dir(), "{} is not a directory", args.dir.display());

    let index: PublicationIndex = serde_json::from_slice(&read_export_file(
        &dir,
        "publication.json",
        MAX_INDEX_BYTES,
    )?)
    .context("publication.json is not a signed-export index; rerun sign-app-update.mjs")?;
    anyhow::ensure!(
        index.app == args.app && index.channel == args.channel,
        "the export was signed for {}/{}, not {}/{}",
        index.app,
        index.channel,
        args.app,
        args.channel
    );

    let manifest_bytes = read_export_file(&dir, "manifest.json", MAX_MANIFEST_BYTES)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .context("manifest.json is not an Expo Updates v1 manifest")?;
    anyhow::ensure!(
        uuid::Uuid::parse_str(&manifest.id).is_ok() && manifest.id == index.id,
        "manifest.json id {} does not match publication.json id {}",
        manifest.id,
        index.id
    );
    anyhow::ensure!(
        manifest.runtime_version == index.runtime_version,
        "manifest.json runtimeVersion {} does not match publication.json {}",
        manifest.runtime_version,
        index.runtime_version
    );
    let referenced = manifest_assets(&manifest, &args.app, &args.channel)?;

    let signature = String::from_utf8(read_export_file(
        &dir,
        "manifest.signature",
        MAX_SIGNATURE_BYTES,
    )?)
    .context("manifest.signature is not UTF-8")?;
    let signature = signature.trim_end_matches(['\r', '\n']).to_owned();
    anyhow::ensure!(
        !signature.is_empty() && !signature.chars().any(char::is_control),
        "manifest.signature must hold one expo-signature header value"
    );

    anyhow::ensure!(
        index.assets.len() <= MAX_ASSETS,
        "the export holds {} assets; the limit is {MAX_ASSETS}",
        index.assets.len()
    );
    let mut total = 0_u64;
    let mut assets = Vec::with_capacity(index.assets.len());
    let mut indexed = BTreeSet::new();
    for asset in index.assets {
        anyhow::ensure!(
            asset.hash.len() == 64
                && asset
                    .hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "publication.json hash {} is not a lowercase hex SHA-256",
            asset.hash
        );
        anyhow::ensure!(
            indexed.insert(asset.hash.clone()),
            "publication.json lists {} twice",
            asset.hash
        );
        let Some(content_type) = referenced.get(&asset.hash) else {
            anyhow::bail!("{} is not referenced by manifest.json", asset.path);
        };
        anyhow::ensure!(
            *content_type == asset.content_type,
            "{} is {} in publication.json but {content_type} in manifest.json",
            asset.path,
            asset.content_type
        );
        let bytes = read_export_file(&dir, &asset.path, MAX_ASSET_BYTES)?;
        total += bytes.len() as u64;
        anyhow::ensure!(
            total <= MAX_TOTAL_ASSET_BYTES,
            "the export holds more than {MAX_TOTAL_ASSET_BYTES} asset bytes"
        );
        let actual = hex::encode(Sha256::digest(&bytes));
        anyhow::ensure!(
            actual == asset.hash,
            "{} has SHA-256 {actual}, but the signed manifest expects {}",
            asset.path,
            asset.hash
        );
        assets.push(PublishAsset {
            hash: asset.hash,
            content_type: asset.content_type,
            bytes: base64::engine::general_purpose::STANDARD.encode(&bytes),
        });
    }
    if let Some(missing) = referenced.keys().find(|hash| !indexed.contains(*hash)) {
        anyhow::bail!(
            "manifest.json references asset {missing} that publication.json does not list"
        );
    }

    Ok(PublishRequest {
        app: args.app.clone(),
        channel: args.channel.clone(),
        manifest: base64::engine::general_purpose::STANDARD.encode(&manifest_bytes),
        signature,
        assets,
        source_ref: args.source_ref.clone(),
        expected_head: args.expected_head.clone(),
    })
}

/// Each manifest asset by hex SHA-256 with its content type; its URL must be the gateway's
/// asset route for this app and channel.
fn manifest_assets(
    manifest: &Manifest,
    app: &str,
    channel: &str,
) -> Result<BTreeMap<String, String>> {
    let mut referenced = BTreeMap::new();
    for asset in std::iter::once(&manifest.launch_asset).chain(&manifest.assets) {
        let digest = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&asset.hash)
            .ok()
            .filter(|digest| digest.len() == 32)
            .with_context(|| {
                format!(
                    "manifest.json hash {} is not a base64url SHA-256",
                    asset.hash
                )
            })?;
        let hex = hex::encode(digest);
        let route = format!(
            "/v1/client/app-updates/assets/{hex}?app={}&channel={}",
            urlencoding::encode(app),
            urlencoding::encode(channel)
        );
        let host = asset
            .url
            .strip_suffix(&route)
            .and_then(|origin| {
                origin
                    .strip_prefix("https://")
                    .or_else(|| origin.strip_prefix("http://"))
            })
            .unwrap_or_default();
        anyhow::ensure!(
            !host.is_empty() && !host.contains(['/', '?', '#', '@']),
            "manifest.json asset URL {} is not <origin>{route}",
            asset.url
        );
        if let Some(previous) = referenced.insert(hex.clone(), asset.content_type.clone()) {
            anyhow::ensure!(
                previous == asset.content_type,
                "manifest.json gives asset {hex} two content types"
            );
        }
    }
    Ok(referenced)
}

/// Read one regular file inside `dir`, refusing absolute paths, `..`, symlinks and files
/// over `limit` bytes.
fn read_export_file(dir: &Path, relative: &str, limit: u64) -> Result<Vec<u8>> {
    let path = Path::new(relative);
    anyhow::ensure!(
        !relative.is_empty()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "export path {relative:?} must stay inside the export directory"
    );
    let mut current = dir.to_path_buf();
    for component in path.components() {
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current)
            .with_context(|| format!("export file {relative}"))?;
        anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "export path {relative} goes through a symlink"
        );
    }
    // O_NOFOLLOW also refuses a final symlink swapped in after the check above.
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&current)
        .with_context(|| format!("export file {relative}"))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file(),
        "export path {relative} is not a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= limit,
        "export file {relative} is {} bytes; the limit is {limit}",
        metadata.len()
    );
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    file.take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= limit,
        "export file {relative} grew past {limit} bytes while it was read"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID: &str = "0194b2e0-1234-7000-8000-000000000001";

    struct Export {
        dir: tempfile::TempDir,
        bundle_hex: String,
    }

    fn asset_json(bytes: &[u8], content_type: &str) -> serde_json::Value {
        let digest = Sha256::digest(bytes);
        json!({
            "hash": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest),
            "key": "k",
            "contentType": content_type,
            "url": format!(
                "https://gateway.example/v1/client/app-updates/assets/{}?app=com.example.app&channel=daily",
                hex::encode(digest)
            ),
        })
    }

    /// A signed export as sign-app-update.mjs writes it; the signature is opaque here.
    fn export() -> Export {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("_expo/static/js/ios")).unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        let bundle = b"bundle bytes";
        let icon = b"png bytes";
        std::fs::write(dir.path().join("_expo/static/js/ios/index.hbc"), bundle).unwrap();
        std::fs::write(dir.path().join("assets/icon"), icon).unwrap();
        let manifest = json!({
            "id": ID,
            "createdAt": "2026-10-07T00:00:00.000Z",
            "runtimeVersion": "1.0.0",
            "launchAsset": asset_json(bundle, "application/javascript"),
            "assets": [asset_json(icon, "image/png")],
            "metadata": {},
            "extra": {},
        });
        std::fs::write(
            dir.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("manifest.signature"),
            "sig=\"AAAA\", keyid=\"main\"",
        )
        .unwrap();
        let bundle_hex = hex::encode(Sha256::digest(bundle));
        let index = json!({
            "app": "com.example.app",
            "channel": "daily",
            "runtimeVersion": "1.0.0",
            "id": ID,
            "assets": [
                { "hash": bundle_hex, "content_type": "application/javascript", "path": "_expo/static/js/ios/index.hbc" },
                { "hash": hex::encode(Sha256::digest(icon)), "content_type": "image/png", "path": "assets/icon" },
            ],
        });
        std::fs::write(
            dir.path().join("publication.json"),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        Export { dir, bundle_hex }
    }

    fn args(dir: &Path) -> PublishArgs {
        PublishArgs {
            app: "com.example.app".into(),
            channel: "daily".into(),
            dir: dir.to_path_buf(),
            source_ref: Some("feature/x".into()),
            expected_head: None,
        }
    }

    fn rewrite_index(dir: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
        let path = dir.join("publication.json");
        let mut index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        edit(&mut index);
        std::fs::write(path, serde_json::to_vec(&index).unwrap()).unwrap();
    }

    #[test]
    fn a_signed_export_becomes_the_exact_publish_request() {
        let export = export();
        let request = prepare(&args(export.dir.path())).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["app"], "com.example.app");
        assert_eq!(body["channel"], "daily");
        assert_eq!(body["signature"], "sig=\"AAAA\", keyid=\"main\"");
        assert_eq!(body["source_ref"], "feature/x");
        assert!(body.get("expected_head").is_none());
        let manifest = base64::engine::general_purpose::STANDARD
            .decode(body["manifest"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            manifest,
            std::fs::read(export.dir.path().join("manifest.json")).unwrap()
        );
        assert_eq!(body["assets"][0]["hash"], export.bundle_hex);
        assert_eq!(body["assets"][0]["content_type"], "application/javascript");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(body["assets"][0]["bytes"].as_str().unwrap())
                .unwrap(),
            b"bundle bytes"
        );
        assert_eq!(body["assets"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn index_paths_cannot_leave_the_export_or_follow_symlinks() {
        let export = export();
        rewrite_index(export.dir.path(), |index| {
            index["assets"][1]["path"] = json!("../icon");
        });
        let error = prepare(&args(export.dir.path())).err().unwrap().to_string();
        assert!(
            error.contains("must stay inside the export directory"),
            "{error}"
        );

        let export = self::export();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("icon"), b"png bytes").unwrap();
        std::fs::remove_dir_all(export.dir.path().join("assets")).unwrap();
        std::os::unix::fs::symlink(outside.path(), export.dir.path().join("assets")).unwrap();
        let error = prepare(&args(export.dir.path())).err().unwrap().to_string();
        assert!(error.contains("goes through a symlink"), "{error}");
    }

    #[test]
    fn changed_bytes_and_unsigned_assets_are_refused() {
        let export = export();
        std::fs::write(export.dir.path().join("assets/icon"), b"other bytes").unwrap();
        let error = prepare(&args(export.dir.path())).err().unwrap().to_string();
        assert!(error.contains("the signed manifest expects"), "{error}");

        let export = self::export();
        std::fs::write(export.dir.path().join("extra.bin"), b"extra").unwrap();
        rewrite_index(export.dir.path(), |index| {
            index["assets"].as_array_mut().unwrap().push(json!({
                "hash": hex::encode(Sha256::digest(b"extra")),
                "content_type": "application/octet-stream",
                "path": "extra.bin",
            }));
        });
        let error = prepare(&args(export.dir.path())).err().unwrap().to_string();
        assert!(error.contains("not referenced by manifest.json"), "{error}");

        let export = self::export();
        rewrite_index(export.dir.path(), |index| {
            index["assets"].as_array_mut().unwrap().pop();
        });
        let error = prepare(&args(export.dir.path())).err().unwrap().to_string();
        assert!(error.contains("does not list"), "{error}");
    }

    #[test]
    fn the_export_must_be_signed_for_the_named_app_and_channel() {
        let export = export();
        let mut wrong = args(export.dir.path());
        wrong.channel = "beta".into();
        let error = prepare(&wrong).err().unwrap().to_string();
        assert!(
            error.contains("signed for com.example.app/daily"),
            "{error}"
        );
    }

    #[test]
    fn source_refs_are_bounded_printable_text() {
        let export = export();
        let mut bad = args(export.dir.path());
        bad.source_ref = Some("main\nx".into());
        assert!(prepare(&bad).is_err());
        bad.source_ref = Some("r".repeat(MAX_SOURCE_REF_CHARS + 1));
        assert!(prepare(&bad).is_err());
        assert!(parse_update_id("not-a-uuid").is_err());
        assert_eq!(parse_update_id(&ID.to_uppercase()).unwrap(), ID);
    }
}
