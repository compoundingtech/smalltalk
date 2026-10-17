//! Message bodies past the inline limit.
//!
//! A message up to [`INLINE_MAX_BYTES`] keeps its text in the `message.sent` claim, as it always
//! has. A longer one, up to [`MAX_BYTES`], keeps a [`preview`] in `content` and keeps the whole
//! text as a file on the member that took the send, its owner: `<state_dir>/message-bodies/<sender>/<id>`.
//! The claim names it in `attachments` by hash, size and origin, as it names an image. A member
//! that reads or delivers the message asks the owner for the text each time, over the signed
//! peer route a relayed client read takes, and keeps no copy. The owner keeps the file as long as
//! it keeps its state; when the owner cannot be reached a reader shows the preview and says where
//! the full text is.
//!
//! Nothing but the preview and a small reference enters the database or replication, and the
//! claim has no new field: an older build keeps only the attachment entries that are images, so
//! it reads the message as its preview.

use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest as _, Sha256};

use crate::model::{MessageAttachment, MessageView, St3Error};

/// The most a message may carry in its claim without a file.
pub const INLINE_MAX_BYTES: usize = 8192;
/// The most a message body may hold, in UTF-8 bytes.
pub const MAX_BYTES: usize = 256 * 1024;
/// The most the claim's `content` holds of a long body.
pub const PREVIEW_BYTES: usize = 1024;
/// The type of the `attachments` entry that is a message's body. No image has it.
pub const MEDIA_TYPE: &str = "text/plain";

/// The most one member keeps as the owner of long bodies, unless `ST3_MESSAGE_BODIES_MAX_MB` says
/// otherwise.
pub const OWNER_STORE_DEFAULT_MB: u64 = 1024;
/// The most one sender may have there, unless `ST3_MESSAGE_BODIES_ACTOR_MAX_MB` says otherwise.
pub const ACTOR_STORE_DEFAULT_MB: u64 = 256;
/// How many days the owner keeps a body unless `ST3_MESSAGE_BODIES_RETENTION_DAYS` says otherwise.
pub const RETENTION_DEFAULT_DAYS: u64 = 30;

/// How long the owner keeps a body: `ST3_MESSAGE_BODIES_RETENTION_DAYS`, 30 days when unset or
/// not a number, and `0` to keep bodies for as long as the owner keeps its state. After that
/// only the preview and the owner's name remain, and a reader that opens the body is told it
/// expired.
pub fn retention() -> Option<Duration> {
    retention_from(
        std::env::var("ST3_MESSAGE_BODIES_RETENTION_DAYS")
            .ok()
            .as_deref(),
    )
}

/// [`retention`] for a setting's value.
pub fn retention_from(setting: Option<&str>) -> Option<Duration> {
    let days = match setting.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.parse::<u64>().unwrap_or(RETENTION_DEFAULT_DAYS),
        None => RETENTION_DEFAULT_DAYS,
    };
    (days > 0).then(|| Duration::from_secs(days.saturating_mul(24 * 3600)))
}

/// How long a body with no claim may sit before reconcile removes it.
const ORPHAN_GRACE: Duration = Duration::from_secs(3600);

/// The owner's long bodies: one file per message, `message-bodies/<sender>/<message id>`, where
/// `<sender>` is the first sixteen hex digits of the SHA-256 of the sender's name. A file is
/// checked against the hash its claim names whenever it is read, so the name is only an address.
/// Keeping them apart from images means nothing sweeps them by age.
pub struct Bodies {
    root: PathBuf,
}

/// One file in the store.
pub struct BodyFile {
    pub actor_dir: String,
    pub id: String,
    pub bytes: u64,
    pub modified: SystemTime,
    /// A write that has not been committed: `.<id>.<n>.part`. It counts against the quotas and
    /// is swept once it is old, and it is never a message's body.
    pub temporary: bool,
    /// The marker `<id>.expired` left where a body aged out, so a reader is told it expired
    /// rather than that it was lost. It holds no text.
    pub expired: bool,
}

/// A body written to its temporary name and not yet renamed into place.
pub struct Staged {
    temporary: PathBuf,
    target: PathBuf,
}

impl Staged {
    /// Make the body visible, atomically, and make the rename durable.
    pub fn commit(self) -> std::io::Result<()> {
        fs::rename(&self.temporary, &self.target)?;
        // A body written again is a live body: it no longer reads as expired.
        if let Some(name) = self.target.file_name() {
            let mut marker = name.to_owned();
            marker.push(".expired");
            let _ = fs::remove_file(self.target.with_file_name(marker));
        }
        if let Some(directory) = self.target.parent() {
            fs::File::open(directory)?.sync_all()?;
        }
        Ok(())
    }

    /// Remove the temporary file: the claim it was for did not land.
    pub fn discard(&self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

fn actor_dir(actor: &str) -> String {
    hex::encode(Sha256::digest(actor.as_bytes()))[..16].to_owned()
}

/// The message id in a `message/ID` subject, if it is a name a file can bear.
fn message_id(subject: &str) -> Option<&str> {
    let id = subject.strip_prefix("message/").unwrap_or(subject);
    (!id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !id.starts_with('.'))
    .then_some(id)
}

/// The owner's store under a state directory.
pub fn bodies(state_dir: &Path) -> Bodies {
    Bodies {
        root: state_dir.join("message-bodies"),
    }
}

impl Bodies {
    fn path(&self, actor: &str, subject: &str) -> Option<PathBuf> {
        Some(self.root.join(actor_dir(actor)).join(message_id(subject)?))
    }

    /// Write one message's body to a temporary name and make it durable, to be committed once the
    /// claim that names it has landed. `None` when the body is already held exactly: nothing to
    /// write. A failed write removes its temporary file.
    pub fn stage(&self, actor: &str, subject: &str, bytes: &[u8]) -> std::io::Result<Option<Staged>> {
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a message name");
        let target = self.path(actor, subject).ok_or_else(invalid)?;
        let directory = target.parent().ok_or_else(invalid)?;
        fs::create_dir_all(directory)?;
        if fs::read(&target).is_ok_and(|held| held == bytes) {
            return Ok(None);
        }
        static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temporary = directory.join(format!(
            ".{}.{}.{}.part",
            message_id(subject).unwrap_or("x"),
            std::process::id(),
            STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let written = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        Ok(Some(Staged { temporary, target }))
    }

    /// Write one message's body, atomically: a reader sees all of it or none.
    pub fn put(&self, actor: &str, subject: &str, bytes: &[u8]) -> std::io::Result<()> {
        match self.stage(actor, subject, bytes)? {
            Some(staged) => staged.commit(),
            None => Ok(()),
        }
    }

    pub fn read(&self, actor: &str, subject: &str) -> std::io::Result<Option<Vec<u8>>> {
        let Some(path) = self.path(actor, subject) else {
            return Ok(None);
        };
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Up to `length` bytes from `offset`, with the file's total size.
    pub fn read_range(
        &self,
        actor: &str,
        subject: &str,
        offset: u64,
        length: usize,
    ) -> std::io::Result<Option<(u64, Vec<u8>)>> {
        let Some(path) = self.path(actor, subject) else {
            return Ok(None);
        };
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let size = file.metadata()?.len();
        file.seek(SeekFrom::Start(offset.min(size)))?;
        let mut bytes = Vec::new();
        file.take(length as u64).read_to_end(&mut bytes)?;
        Ok(Some((size, bytes)))
    }

    /// Whether `file` is the one this store keeps for `actor`'s message of that name.
    pub fn holds(&self, actor: &str, file: &BodyFile) -> bool {
        file.actor_dir == actor_dir(actor)
    }

    /// Whether this body aged out here, so it is gone on purpose and only its preview remains.
    pub fn is_expired(&self, actor: &str, subject: &str) -> bool {
        self.path(actor, subject).is_some_and(|path| {
            let mut name = path.file_name().map(|name| name.to_owned()).unwrap_or_default();
            name.push(".expired");
            path.with_file_name(name).is_file()
        })
    }

    /// Remove a body and leave the marker that says it aged out, in that order of intent: the
    /// marker is written first so a reader never sees neither.
    fn expire(&self, file: &BodyFile) {
        let directory = self.root.join(&file.actor_dir);
        if fs::write(directory.join(format!("{}.expired", file.id)), b"").is_ok() {
            let _ = fs::remove_file(directory.join(&file.id));
        }
    }

    pub fn exists(&self, actor: &str, subject: &str) -> bool {
        self.path(actor, subject).is_some_and(|path| path.is_file())
    }

    pub fn remove(&self, actor: &str, subject: &str) {
        if let Some(path) = self.path(actor, subject) {
            let _ = fs::remove_file(path);
        }
    }

    /// Every body file, with its sender's directory name and age.
    pub fn files(&self) -> Vec<BodyFile> {
        let mut files = Vec::new();
        let Ok(actors) = fs::read_dir(&self.root) else {
            return files;
        };
        for actor in actors.flatten() {
            let Some(actor_dir) = actor.file_name().into_string().ok() else {
                continue;
            };
            let Ok(entries) = fs::read_dir(actor.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(metadata) = entry.metadata() else { continue };
                let Some(name) = entry.file_name().into_string().ok() else {
                    continue;
                };
                let temporary = name.starts_with('.') && name.ends_with(".part");
                let marker = name
                    .strip_suffix(".expired")
                    .filter(|id| message_id(id).is_some())
                    .map(str::to_owned);
                if !metadata.is_file()
                    || !(temporary || marker.is_some() || message_id(&name).is_some())
                {
                    continue;
                }
                let expired = marker.is_some();
                files.push(BodyFile {
                    actor_dir: actor_dir.clone(),
                    id: marker.unwrap_or(name),
                    bytes: if expired { 0 } else { metadata.len() },
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    temporary,
                    expired,
                });
            }
        }
        files
    }

    /// Whether this member may take a body of `size` bytes for `subject` from `actor`: the one it
    /// already holds costs nothing, and the rest must fit under the limit on what a sender may
    /// have and the limit on what the member keeps.
    pub fn ensure_room(&self, actor: &str, subject: &str, size: usize) -> Result<(), St3Error> {
        let limit = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|mb| mb.parse::<u64>().ok())
                .filter(|mb| *mb > 0)
                .unwrap_or(default)
                .saturating_mul(1024 * 1024)
        };
        let (total_limit, actor_limit) = (
            limit("ST3_MESSAGE_BODIES_MAX_MB", OWNER_STORE_DEFAULT_MB),
            limit("ST3_MESSAGE_BODIES_ACTOR_MAX_MB", ACTOR_STORE_DEFAULT_MB),
        );
        let (mine, id) = (actor_dir(actor), message_id(subject).unwrap_or_default().to_owned());
        let (mut total, mut sender) = (0_u64, 0_u64);
        for file in self.files() {
            if !file.temporary && !file.expired && file.actor_dir == mine && file.id == id {
                return Ok(());
            }
            total += file.bytes;
            if file.actor_dir == mine {
                sender += file.bytes;
            }
        }
        let size = size as u64;
        if sender.saturating_add(size) > actor_limit {
            return Err(St3Error::new(
                "message-store-full",
                format!(
                    "{actor} already has {} MiB of long messages on this machine, the most it keeps for one sender; send a shorter message",
                    sender / (1024 * 1024)
                ),
            ));
        }
        if total.saturating_add(size) > total_limit {
            return Err(St3Error::new(
                "message-store-full",
                format!(
                    "this machine already keeps {} MiB of long messages, its limit; send a shorter message or ask whoever runs it to raise ST3_MESSAGE_BODIES_MAX_MB",
                    total / (1024 * 1024)
                ),
            ));
        }
        Ok(())
    }

    /// Keep the store to what is named and what is young enough. Returns how many files it
    /// removed or expired.
    ///
    /// - A temporary file older than an hour is a write that never committed: removed.
    /// - A body whose message the store says does not exist, older than an hour, is removed; a
    ///   file younger than an hour is left alone so a send in flight is never taken.
    /// - A body older than `retention` is expired: its text is removed and a marker left, so
    ///   the message reads as its preview with a typed "expired" state.
    /// - A marker whose message no longer exists is removed.
    pub fn reconcile(
        &self,
        retention: Option<Duration>,
        names_a_body: impl Fn(&BodyFile) -> bool,
    ) -> usize {
        let mut changed = 0;
        for file in self.files() {
            let age = file.modified.elapsed().ok();
            let old = age.is_some_and(|age| age > ORPHAN_GRACE);
            let path = self.root.join(&file.actor_dir);
            if file.expired {
                if old && !names_a_body(&file) {
                    let _ = fs::remove_file(path.join(format!("{}.expired", file.id)));
                    changed += 1;
                }
            } else if file.temporary {
                if old {
                    let _ = fs::remove_file(path.join(&file.id));
                    changed += 1;
                }
            } else if old && !names_a_body(&file) {
                let _ = fs::remove_file(path.join(&file.id));
                changed += 1;
            } else if retention.is_some_and(|keep| age.is_some_and(|age| age > keep)) {
                self.expire(&file);
                changed += 1;
            }
        }
        changed
    }
}

/// The start of `text`, cut at a character boundary and marked as cut.
pub fn preview(text: &str) -> String {
    let mut end = PREVIEW_BYTES.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = text[..end].trim_end().to_owned();
    preview.push('…');
    preview
}

/// The reference a message view gives for its body: `blob/<sha256>`.
pub fn reference(hash: &str) -> String {
    format!("blob/{hash}")
}

/// The hash in a body reference, if it is one.
pub fn parse_reference(reference: &str) -> Option<&str> {
    let hash = reference.strip_prefix("blob/")?;
    crate::blobs::is_sha256(hash).then_some(hash)
}

/// A long body as the attachment record it is kept as. st3-kernel defines `MessageView`; the
/// body reference format lives here, so the method is an extension trait.
pub trait BodyAttachment {
    /// A long body as the attachment record it is kept as: a file on the member that took the
    /// send, fetched from there like an image.
    fn body_attachment(&self) -> Option<MessageAttachment>;
}

impl BodyAttachment for MessageView {
    fn body_attachment(&self) -> Option<MessageAttachment> {
        Some(MessageAttachment {
            sha256: parse_reference(self.body_ref.as_deref()?)?.to_owned(),
            media_type: MEDIA_TYPE.into(),
            name: None,
            size: self.body_bytes?,
            origin: self.body_origin.clone()?,
        })
    }
}

/// The images and the body among a claim's `attachments`. Only references a file name can be
/// built from count, and a body must be of a plausible size, so a hostile entry never reaches a
/// path, a URL or a read.
pub fn split_attachments(
    attachments: Option<&serde_json::Value>,
) -> (
    Vec<crate::model::MessageAttachment>,
    Option<crate::model::MessageAttachment>,
) {
    // The body is the entry the signature judge takes, chosen by one function in smallclaims.
    let body = attachments
        .and_then(smallclaims::principal::message_body_entry)
        .and_then(|entry| {
            serde_json::from_value::<crate::model::MessageAttachment>(entry.clone()).ok()
        });
    let entries = attachments
        .cloned()
        .and_then(|value| {
            serde_json::from_value::<Vec<crate::model::MessageAttachment>>(value).ok()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|attachment| {
            crate::blobs::is_sha256(&attachment.sha256) && attachment.origin.starts_with("host/")
        });
    let mut images = Vec::new();
    for attachment in entries {
        if attachment.media_type != MEDIA_TYPE
            && crate::blobs::MEDIA_TYPES.contains(&attachment.media_type.as_str())
            && images.len() < crate::blobs::MAX_ATTACHMENTS
        {
            images.push(attachment);
        }
    }
    (images, body)
}

/// The whole text of a message: its own content, unless that is only the preview of a body kept
/// as a file, which the daemon reads out for it. A body whose owner cannot be reached reads as
/// its preview with a note saying where the full text is, so a seat still gets the message.
pub async fn full_text(
    client: &crate::client::Client,
    message: &crate::model::MessageView,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    if message.body_ref.is_none() {
        return Ok(message.content.clone());
    }
    let value: serde_json::Value = client
        .get(&format!("/v1/messages/body/{}", message.subject))
        .await?;
    Ok(value["text"]
        .as_str()
        .context("the message body answer has no text")?
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessageAttachment;
    use serde_json::json;

    #[test]
    fn a_preview_is_cut_on_a_character_and_marked() {
        let text = "é".repeat(PREVIEW_BYTES);
        let cut = preview(&text);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= PREVIEW_BYTES + '…'.len_utf8());
        assert!(text.starts_with(cut.trim_end_matches('…')));
        assert_eq!(preview("short"), "short…");
    }

    #[test]
    fn the_clients_cut_the_preview_the_way_the_daemon_does() {
        // A device signs the preview it computes itself, so the two must agree to the byte,
        // including at a character boundary and on trailing whitespace.
        assert_eq!(INLINE_MAX_BYTES, st3_client::device::INLINE_MAX_BYTES);
        let texts = [
            "short".to_owned(),
            "é".repeat(2000),
            "€".repeat(1000),
            format!("{}   \n\u{a0}\t{}", "a".repeat(1021), "tail"),
            format!("{}{}", "a".repeat(1023), "🎉🎉"),
            "line of notes\n".repeat(900),
        ];
        for text in texts {
            assert_eq!(
                preview(&text),
                st3_client::device::long_message_preview(&text)
            );
        }
    }

    #[test]
    fn only_a_hash_is_a_body_reference() {
        let hash = "a".repeat(64);
        assert_eq!(parse_reference(&reference(&hash)), Some(hash.as_str()));
        assert_eq!(parse_reference("doc/x@aa"), None);
        assert_eq!(parse_reference(&format!("blob/{}", "A".repeat(64))), None);
    }

    #[test]
    fn the_body_is_told_from_the_images_and_hostile_entries_are_dropped() {
        let entry = |hash: String, media: &str, size: u64, origin: &str| {
            json!(MessageAttachment {
                sha256: hash,
                media_type: media.into(),
                name: None,
                size,
                origin: origin.into(),
            })
        };
        let good = "a".repeat(64);
        let entries = json!([
            entry("b".repeat(64), "image/png", 10, "host/one"),
            entry(good.clone(), MEDIA_TYPE, 5000, "host/one"),
            entry("../x".into(), MEDIA_TYPE, 5000, "host/one"),
            entry("c".repeat(64), MEDIA_TYPE, 5000, "elsewhere"),
            entry("d".repeat(64), MEDIA_TYPE, MAX_BYTES as u64 + 1, "host/one"),
            entry("e".repeat(64), "application/x-sh", 5, "host/one"),
        ]);
        let (images, body) = split_attachments(Some(&entries));
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(body.unwrap().sha256, good);
        assert_eq!(split_attachments(None), (Vec::new(), None));
    }
}
