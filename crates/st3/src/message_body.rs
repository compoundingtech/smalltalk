//! Message bodies past the inline limit.
//!
//! A message up to [`INLINE_MAX_BYTES`] keeps its text in the `message.sent` claim, as it always
//! has. A longer one, up to [`MAX_BYTES`], keeps a [`preview`] in `content` and keeps the whole
//! text as a file on the member that took the send, its owner: `<state_dir>/message-bodies/<sha256>`.
//! The claim names it in `attachments` by hash, size and origin, as it names an image. A member
//! that reads or delivers the message asks the owner for the text each time, over the signed
//! peer route a relayed client read takes, and keeps no copy. The owner keeps the file as long as
//! it keeps its state; when the owner cannot be reached a reader shows the preview and says where
//! the full text is.
//!
//! Nothing but the preview and a small reference enters the database or replication, and the
//! claim has no new field: an older build keeps only the attachment entries that are images, so
//! it reads the message as its preview.

use std::path::Path;

/// The owner's directory of long bodies, kept apart from images so that nothing sweeps it.
pub fn directory(state_dir: &Path) -> crate::blobs::BlobDir {
    crate::blobs::BlobDir::at(state_dir.join("message-bodies"))
}

/// The most a message may carry in its claim without a file.
pub const INLINE_MAX_BYTES: usize = 4096;
/// The most a message body may hold, in UTF-8 bytes.
pub const MAX_BYTES: usize = 256 * 1024;
/// The most the claim's `content` holds of a long body.
pub const PREVIEW_BYTES: usize = 1024;
/// The type of the `attachments` entry that is a message's body. No image has it.
pub const MEDIA_TYPE: &str = "text/plain";

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
