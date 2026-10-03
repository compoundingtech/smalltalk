//! Attachment files: pasted images a message carries from one machine to another.
//!
//! The bytes are files under `<state_dir>/blobs`, named by their SHA-256. They are never claims
//! and never replicated: a message claim holds only the reference and the member that took the
//! upload, and the members that read or deliver the message fetch the bytes from that member, one
//! direct hop at a time, and keep a copy here. The daemon deletes files older than the retention
//! window; a read after that answers `blob-expired` and the message keeps its text.

use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest as _, Sha256};

use crate::model::St3Error;

/// The most one upload may hold.
pub const MAX_BLOB_BYTES: usize = 10 * 1024 * 1024;
/// The most files one message may carry.
pub const MAX_ATTACHMENTS: usize = 4;
/// The most one actor may hold in unexpired uploads.
pub const UPLOAD_QUOTA_BYTES: u64 = 128 * 1024 * 1024;
/// How long a file is kept, unless the configuration says otherwise.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// The most one relayed read carries. Base64 inflates it by a third, and a relayed answer is
/// bounded at 1 MiB, so a file moves between members in chunks of this size.
pub const CHUNK_BYTES: usize = 512 * 1024;

/// The types a message may carry, each checked against the file's own first bytes.
pub const MEDIA_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

pub fn file_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// The type the bytes themselves declare, if they are one of the supported images.
pub fn sniff_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// `blob/<sha256>` or a bare hash, as the hash.
pub fn parse_reference(value: &str) -> Result<String, St3Error> {
    let hash = value.strip_prefix("blob/").unwrap_or(value).to_ascii_lowercase();
    if is_sha256(&hash) {
        Ok(hash)
    } else {
        Err(St3Error::new(
            "invalid-blob-reference",
            "a blob reference is `blob/` and 64 hex digits",
        ))
    }
}

/// Check an upload's type and bytes; return the type to record.
pub fn validate_upload(media_type: &str, bytes: &[u8]) -> Result<&'static str, St3Error> {
    let declared = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let Some(supported) = MEDIA_TYPES.iter().find(|candidate| **candidate == declared) else {
        return Err(St3Error::new(
            "unsupported-media-type",
            format!("an attachment is one of {}", MEDIA_TYPES.join(", ")),
        ));
    };
    if bytes.is_empty() {
        return Err(St3Error::new("blob-content-mismatch", "the upload is empty"));
    }
    if bytes.len() > MAX_BLOB_BYTES {
        return Err(St3Error::new(
            "blob-too-large",
            format!("an attachment is at most {MAX_BLOB_BYTES} bytes"),
        ));
    }
    if sniff_media_type(bytes) != Some(*supported) {
        return Err(St3Error::new(
            "blob-content-mismatch",
            format!("the bytes are not {supported}"),
        ));
    }
    Ok(supported)
}

/// The directory of attachment files under a daemon's state directory.
#[derive(Clone, Debug)]
pub struct BlobDir {
    root: PathBuf,
}

impl BlobDir {
    pub fn under(state_dir: &Path) -> Self {
        Self {
            root: state_dir.join("blobs"),
        }
    }

    /// A directory of files kept by hash, such as the one a seat's driver writes for its harness.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn path(&self, hash: &str) -> PathBuf {
        self.root.join(hash)
    }

    fn ensure(&self) -> std::io::Result<()> {
        if !self.root.is_dir() {
            fs::create_dir_all(&self.root)?;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Store bytes under their hash, atomically. Storing again refreshes the file's age.
    pub fn put(&self, bytes: &[u8]) -> std::io::Result<String> {
        self.ensure()?;
        let hash = hex::encode(Sha256::digest(bytes));
        let path = self.path(&hash);
        if path.is_file() {
            touch(&path);
            return Ok(hash);
        }
        let temporary = self.root.join(format!(".{hash}.{}.part", std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        Ok(hash)
    }

    pub fn size(&self, hash: &str) -> Option<u64> {
        fs::metadata(self.path(hash))
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
    }

    pub fn read(&self, hash: &str) -> std::io::Result<Option<Vec<u8>>> {
        match fs::read(self.path(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Up to `length` bytes from `offset`, with the file's total size.
    pub fn read_range(
        &self,
        hash: &str,
        offset: u64,
        length: usize,
    ) -> std::io::Result<Option<(u64, Vec<u8>)>> {
        let mut file = match fs::File::open(self.path(hash)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let size = file.metadata()?.len();
        file.seek(SeekFrom::Start(offset.min(size)))?;
        let mut bytes = vec![0; length.min(CHUNK_BYTES)];
        let mut filled = 0;
        while filled < bytes.len() {
            let read = file.read(&mut bytes[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        bytes.truncate(filled);
        Ok(Some((size, bytes)))
    }

    /// Delete files not written for `retention`; the number deleted.
    pub fn sweep(&self, retention: Duration) -> usize {
        let Some(cutoff) = SystemTime::now().checked_sub(retention) else {
            return 0;
        };
        let Ok(entries) = fs::read_dir(&self.root) else {
            return 0;
        };
        let mut deleted = 0;
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // A half-written upload older than the window is debris too.
            if (is_sha256(name.split('.').next().unwrap_or_default()) || name.ends_with(".part"))
                && metadata.is_file()
                && metadata.modified().is_ok_and(|modified| modified < cutoff)
                && fs::remove_file(entry.path()).is_ok()
            {
                deleted += 1;
            }
        }
        deleted
    }
}

/// Metadata and a safe, exact CLI retrieval command, even when no file was materialized.
pub fn attachment_notices(
    message: &crate::model::MessageView,
) -> Vec<st_drivers::ding::AttachmentNotice> {
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\"'\"'"));
    message
        .attachments
        .iter()
        .map(|attachment| {
            let output = format!(
                "{}.{}",
                attachment.sha256,
                file_extension(&attachment.media_type)
            );
            st_drivers::ding::AttachmentNotice {
                path: None,
                media_type: attachment.media_type.clone(),
                name: attachment.name.clone(),
                size: attachment.size,
                unavailable: None,
                fetch_command: Some(format!(
                    "st blobs get {} --message {} -o {}",
                    quote(&format!("blob/{}", attachment.sha256)),
                    quote(&message.subject),
                    quote(&output)
                )),
            }
        })
        .collect()
}

/// A transport-only body notice for old drivers. The durable graph message stays unchanged.
pub fn annotate_delivery(
    store: &crate::store::Store,
    message: &mut crate::model::MessageView,
) -> anyhow::Result<()> {
    if message.attachments.is_empty() {
        return Ok(());
    }
    let body = if message.content.starts_with("doc/") {
        let (name, hash) = message
            .content
            .rsplit_once('@')
            .ok_or_else(|| anyhow::anyhow!("invalid message document reference"))?;
        String::from_utf8(
            store
                .get_document(name, hash)?
                .ok_or_else(|| anyhow::anyhow!("message document is not stored"))?,
        )?
    } else {
        message.content.clone()
    };
    // Keep the exact graph-body digest alongside its decorated delivery copy.
    message
        .tags
        .retain(|tag| !tag.starts_with(st_drivers::ding::ST3_SHA256_TAG));
    message.tags.push(format!(
        "{}{}",
        st_drivers::ding::ST3_SHA256_TAG,
        st_drivers::ding::st3_body_sha256(&body)
    ));
    message.content = format!(
        "{}\n{body}",
        st_drivers::ding::attachment_summary(&attachment_notices(message))
    );
    Ok(())
}

pub fn delivery_body_sha256(message: &crate::model::MessageView, body: &str) -> String {
    if !message.attachments.is_empty()
        && let Some(hash) = message
            .tags
            .iter()
            .find_map(|tag| tag.strip_prefix(st_drivers::ding::ST3_SHA256_TAG))
            .filter(|hash| is_sha256(hash))
    {
        return hash.into();
    }
    st_drivers::ding::st3_body_sha256(body)
}

/// Write each available image locally. Failed files remain visible as notices, with a fetch
/// command; they do not hold back the text or other images in the same message.
pub async fn materialize(
    socket: &Path,
    actor: &str,
    directory: &Path,
    message: &crate::model::MessageView,
) -> anyhow::Result<Vec<st_drivers::ding::AttachmentNotice>> {
    let mut notices = attachment_notices(message);
    if notices.is_empty() {
        return Ok(notices);
    }
    let files = BlobDir::at(directory.to_path_buf());
    files.sweep(DEFAULT_RETENTION);
    let client = st3_client::Client::unix_as(socket, actor);
    for (attachment, notice) in message.attachments.iter().zip(&mut notices) {
        let result: anyhow::Result<PathBuf> = async {
            anyhow::ensure!(is_sha256(&attachment.sha256), "invalid attachment hash");
            files.ensure()?;
            let path = directory.join(format!(
                "{}.{}",
                attachment.sha256,
                file_extension(&attachment.media_type)
            ));
            if fs::metadata(&path).is_ok_and(|metadata| metadata.len() == attachment.size) {
                touch(&path);
                return Ok(path);
            }
            let bytes = client
                .blob(&attachment.sha256, Some(&message.subject))
                .await?;
            anyhow::ensure!(
                hex::encode(Sha256::digest(&bytes)) == attachment.sha256,
                "attachment bytes do not match their hash"
            );
            let temporary = directory.join(format!(
                ".{}.{}.part",
                attachment.sha256,
                std::process::id()
            ));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            Ok(path)
        }
        .await;
        match result {
            Ok(path) => notice.path = Some(path.display().to_string()),
            Err(error) => notice.unavailable = Some(format!("{error}")),
        }
    }
    Ok(notices)
}

/// [`materialize`] for a native seat. Every attachment gets a notice even on transport failure.
pub async fn materialize_for_seat(
    client: &crate::client::Client,
    subject: &str,
    directory: &Path,
    message: &crate::model::MessageView,
) -> Vec<st_drivers::ding::AttachmentNotice> {
    if message.attachments.is_empty() {
        return Vec::new();
    }
    let result = match client.socket_path() {
        Some(socket) => materialize(socket, subject, directory, message).await,
        None => Err(anyhow::anyhow!(
            "attachments are read over the daemon's Unix socket"
        )),
    };
    result.unwrap_or_else(|error| {
        let mut notices = attachment_notices(message);
        for notice in &mut notices {
            notice.unavailable = Some(error.to_string());
        }
        notices
    })
}

/// Image blocks accepted by the pi-family extension API. No unavailable file becomes an image.
pub fn image_parts(attachments: &[st_drivers::ding::AttachmentNotice]) -> Vec<serde_json::Value> {
    use base64::Engine as _;
    attachments.iter().filter(|attachment| attachment.unavailable.is_none()).filter_map(|attachment| {
        let bytes = fs::read(attachment.path.as_ref()?).ok()?;
        Some(serde_json::json!({"type":"image", "data":base64::engine::general_purpose::STANDARD.encode(bytes), "mimeType":attachment.media_type}))
    }).collect()
}

fn touch(path: &Path) {
    if let Ok(file) = fs::OpenOptions::new().append(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n0000";

    fn image_message(body: &str) -> crate::model::MessageView {
        crate::model::MessageView {
            subject: "message/image-only".into(),
            from: "person/example".into(),
            to: "agent/eval.worker".into(),
            content: body.into(),
            status: "sent".into(),
            title: None,
            in_reply_to: None,
            tags: Vec::new(),
            created_index: 1,
            attachments: vec![crate::model::MessageAttachment {
                sha256: hex::encode(Sha256::digest(PNG)),
                media_type: "image/png".into(),
                name: Some("snapshot.png".into()),
                size: PNG.len() as u64,
                origin: "host/bluey".into(),
            }],
        }
    }

    #[test]
    fn image_only_delivery_is_visible_to_an_old_driver_without_changing_the_graph_body() {
        for body in [String::new(), "long text ".repeat(200)] {
            let store = crate::store::Store::open_memory("node").unwrap();
            let document = store
                .put_document(
                    "doc/attachment-body",
                    body.as_bytes(),
                    &None,
                    &format!("body-{}", body.len()),
                )
                .unwrap();
            let mut message = image_message(&format!("doc/attachment-body@{}", document.hash));
            annotate_delivery(&store, &mut message).unwrap();
            // A pre-attachment MessageView ignores unknown JSON fields but still reads content.
            #[derive(serde::Deserialize)]
            struct OldMessage {
                content: String,
            }
            let old: OldMessage =
                serde_json::from_value(serde_json::to_value(&message).unwrap()).unwrap();
            let preview = st_drivers::ding::st3_notification_text(
                &message.subject,
                &message.from,
                &message.to,
                None,
                &old.content,
                "digest",
            );
            let ping = st_drivers::ding::st3_ping_text(
                &message.subject,
                &message.from,
                None,
                &old.content,
            );
            assert!(preview.contains("1 attachments"), "{preview}");
            assert!(
                ping.lines().next().unwrap().contains("1 attachments"),
                "{ping}"
            );
            assert!(preview.contains("snapshot.png"));
            assert!(preview.contains("st blobs get"));
            assert!(message.content.ends_with(&body));
            assert_eq!(
                delivery_body_sha256(&message, &message.content),
                st_drivers::ding::st3_body_sha256(&body)
            );
            assert_eq!(
                store
                    .get_document("doc/attachment-body", &document.hash)
                    .unwrap()
                    .unwrap(),
                body.as_bytes()
            );
        }
    }

    #[tokio::test]
    async fn a_failed_attachment_fetch_delivers_a_notice_and_keeps_other_images() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("attachments");
        fs::create_dir_all(&directory).unwrap();
        let mut message = image_message("");
        let first = message.attachments[0].clone();
        let path = directory.join(format!("{}.png", first.sha256));
        fs::write(&path, PNG).unwrap();
        let mut missing = first;
        missing.sha256 = "0".repeat(64);
        message.attachments.push(missing);
        let client = crate::client::Client::unix(root.path().join("absent.sock"));
        let notices = materialize_for_seat(&client, &message.to, &directory, &message).await;
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[0].path.as_deref(), path.to_str());
        assert!(notices[0].unavailable.is_none());
        assert!(notices[1].path.is_none());
        assert!(notices[1].unavailable.is_some());
        assert!(
            notices[1]
                .fetch_command
                .as_ref()
                .unwrap()
                .contains("--message 'message/image-only'")
        );
        let envelope = st_drivers::ding::st3_notification_with_attachments(
            &message.subject,
            &message.from,
            &message.to,
            None,
            "",
            "hash",
            &notices,
        );
        assert!(envelope.contains("2 attachments"));
        assert!(envelope.contains("fetch_command="));
        assert!(envelope.contains("unavailable="));
        assert_eq!(image_parts(&notices).len(), 1);
        let summary = st_drivers::ding::attachment_summary(&notices);
        assert!(summary.contains("attachment could not be fetched"));
    }

    #[test]
    fn uploads_are_checked_against_their_own_bytes() {
        assert_eq!(validate_upload("image/png", PNG).unwrap(), "image/png");
        assert_eq!(
            validate_upload("image/png; charset=binary", PNG).unwrap(),
            "image/png"
        );
        assert_eq!(
            validate_upload("image/jpeg", PNG).unwrap_err().code,
            "blob-content-mismatch"
        );
        assert_eq!(
            validate_upload("text/plain", PNG).unwrap_err().code,
            "unsupported-media-type"
        );
        assert_eq!(
            validate_upload("image/png", &[]).unwrap_err().code,
            "blob-content-mismatch"
        );
        let mut large = PNG.to_vec();
        large.resize(MAX_BLOB_BYTES + 1, 0);
        assert_eq!(
            validate_upload("image/png", &large).unwrap_err().code,
            "blob-too-large"
        );
    }

    #[test]
    fn files_are_named_by_hash_ranged_and_swept() {
        let directory = tempfile::tempdir().unwrap();
        let blobs = BlobDir::under(directory.path());
        let hash = blobs.put(PNG).unwrap();
        assert_eq!(hash, hex::encode(Sha256::digest(PNG)));
        assert_eq!(blobs.put(PNG).unwrap(), hash);
        assert_eq!(blobs.size(&hash), Some(PNG.len() as u64));
        assert_eq!(blobs.read(&hash).unwrap().unwrap(), PNG);
        let (size, tail) = blobs.read_range(&hash, 8, 100).unwrap().unwrap();
        assert_eq!((size, tail.as_slice()), (PNG.len() as u64, &PNG[8..]));
        assert!(blobs.read_range(&"0".repeat(64), 0, 1).unwrap().is_none());
        assert_eq!(blobs.sweep(Duration::from_secs(3600)), 0);
        assert_eq!(blobs.sweep(Duration::ZERO), 1);
        assert_eq!(blobs.size(&hash), None);
    }

    #[test]
    fn references_name_a_hash() {
        let hash = "ab".repeat(32);
        assert_eq!(parse_reference(&format!("blob/{hash}")).unwrap(), hash);
        assert_eq!(parse_reference(&hash).unwrap(), hash);
        assert!(parse_reference("blob/xyz").is_err());
    }
}
