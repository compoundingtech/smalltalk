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

use crate::model::St3Error;

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

/// Remove the bodies in `state_dir` that no message claim here names for their sender, now and
/// once an hour while the daemon runs. A body is written only after its claim, so what this
/// finds is a body whose claim a checkpoint dropped, never a send in flight.
pub fn spawn_reconciler(store: std::sync::Arc<crate::store::Store>, state_dir: PathBuf) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            let (store, state_dir) = (store.clone(), state_dir.clone());
            let _ = tokio::task::spawn_blocking(move || {
                let bodies = bodies(&state_dir);
                bodies.reconcile(|file| {
                    store
                        .message(&format!("message/{}", file.id))
                        .ok()
                        .flatten()
                        .is_some_and(|message| {
                            message.body_ref.is_some() && actor_dir(&message.from) == file.actor_dir
                        })
                });
            })
            .await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
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

    /// Write one message's body, atomically: a reader sees all of it or none. A repeat of the same
    /// text only touches the file.
    pub fn put(&self, actor: &str, subject: &str, bytes: &[u8]) -> std::io::Result<()> {
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a message name");
        let path = self.path(actor, subject).ok_or_else(invalid)?;
        let directory = path.parent().ok_or_else(invalid)?;
        fs::create_dir_all(directory)?;
        if fs::read(&path).is_ok_and(|held| held == bytes) {
            return Ok(());
        }
        let temporary = directory.join(format!(".{}.{}.part", message_id(subject).unwrap_or("x"), std::process::id()));
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)
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
                let Some(id) = entry.file_name().into_string().ok().filter(|id| message_id(id).is_some()) else {
                    continue;
                };
                if metadata.is_file() {
                    files.push(BodyFile {
                        actor_dir: actor_dir.clone(),
                        id,
                        bytes: metadata.len(),
                        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    });
                }
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
            if file.actor_dir == mine && file.id == id {
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

    /// Remove the bodies whose messages no claim here names: those a checkpoint dropped, or
    /// left by a write that never got its claim. A file younger than an hour is left alone, so
    /// a send in flight is never taken. Returns how many were removed.
    pub fn reconcile(&self, names_a_body: impl Fn(&BodyFile) -> bool) -> usize {
        let mut removed = 0;
        for file in self.files() {
            let old = file.modified.elapsed().map_or(false, |age| age > ORPHAN_GRACE);
            if old && !names_a_body(&file) {
                let _ = fs::remove_file(self.root.join(&file.actor_dir).join(&file.id));
                removed += 1;
            }
        }
        removed
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

/// The images and the body among a claim's `attachments`. Only references a file name can be
/// built from count, and a body must be of a plausible size, so a hostile entry never reaches a
/// path, a URL or a read.
pub fn split_attachments(
    attachments: Option<&serde_json::Value>,
) -> (
    Vec<crate::model::MessageAttachment>,
    Option<crate::model::MessageAttachment>,
) {
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
    let mut body = None;
    for attachment in entries {
        if attachment.media_type == MEDIA_TYPE {
            if body.is_none() && (1..=MAX_BYTES as u64).contains(&attachment.size) {
                body = Some(attachment);
            }
        } else if crate::blobs::MEDIA_TYPES.contains(&attachment.media_type.as_str())
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
