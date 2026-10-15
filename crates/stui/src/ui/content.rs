//! On-demand conversation content. Neither bytes nor terminal image protocols leave memory.
//!
//! An image is read, decoded and scaled down to at most [`MAX_EDGE`] pixels on a worker, never on
//! the UI thread, and its terminal encoding is built off the draw path (`Content::protocol_jobs`). Its
//! pixels and encodings are released once it is far from every viewport; its type, size and
//! dimensions stay, so the line still says what it is.

use super::doc::{Doc, Hit, Target};
use super::{text, theme};
use base64::Engine as _;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
};
use serde_json::Value;
use st3_client::{Client, ConversationContentRef, TimelineBody};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
};

/// An image encoded for the terminal at the size it is drawn, sliced by rows so an image the
/// viewport cuts draws its visible part.
pub type Encoding = ratatui_image::sliced::SlicedProtocol;

/// Encode `image` for `width` x `height` cells: on a worker, never on the draw path.
pub fn encode(
    picker: &ratatui_image::picker::Picker,
    image: &image::DynamicImage,
    width: u16,
    height: u16,
) -> Option<Encoding> {
    Encoding::new_with_resize(
        picker,
        image.clone(),
        ratatui::layout::Size::new(width, height),
        ratatui_image::Resize::Fit(None),
    )
    .ok()
}

/// The longest side an image is kept at. A terminal pane is rarely wider than this in pixels,
/// and a 6000×4000 photo would otherwise hold 72 MB.
pub const MAX_EDGE: u32 = 1600;
/// The largest picture decoded at all, by edge and by memory, before it is scaled to [`MAX_EDGE`]:
/// a 6000×4000 photo fits, a file declaring 100000×100000 pixels is refused unread.
const MAX_DECODED_EDGE: u32 = 16_384;
const MAX_DECODED_BYTES: u64 = 192 * 1024 * 1024;
/// The rows an inline image reserves under its line.
pub const IMAGE_ROWS: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub conversation: String,
    pub entry: String,
    pub revision: u32,
    pub reference: String,
}

struct Group {
    block_index: usize,
    refs: Vec<(Key, ConversationContentRef)>,
}

enum Loaded {
    Loading,
    Failed(String),
    Json {
        presentation: Presentation,
        images: Vec<ConversationContentRef>,
    },
    Image {
        media_type: String,
        size: u64,
        image: Arc<image::DynamicImage>,
    },
}

/// What a read produced, decoded off the UI thread.
pub enum Fetched {
    Json(Vec<u8>),
    Image {
        media_type: String,
        /// The bytes as sent.
        size: u64,
        /// The image as sent, before scaling.
        width: u32,
        height: u32,
        image: image::DynamicImage,
    },
}

impl Fetched {
    /// Decode an image and scale it to [`MAX_EDGE`] before anything holds it.
    pub fn decode(bytes: Vec<u8>) -> Result<Self, String> {
        let size = bytes.len() as u64;
        // The dimensions an image declares are checked before any pixel is allocated: a small file
        // may declare a huge picture.
        let decoded = image::guess_format(&bytes).and_then(|format| {
            let mut reader = image::ImageReader::with_format(std::io::Cursor::new(&bytes), format);
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(MAX_DECODED_EDGE);
            limits.max_image_height = Some(MAX_DECODED_EDGE);
            limits.max_alloc = Some(MAX_DECODED_BYTES);
            reader.limits(limits);
            reader.decode().map(|image| (format, image))
        });
        drop(bytes);
        let (format, image) = decoded.map_err(|error| format!("Cannot display this image: {error}"))?;
        let (width, height) = (image.width(), image.height());
        let image = if width > MAX_EDGE || height > MAX_EDGE {
            image.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Triangle)
        } else {
            image
        };
        Ok(Self::Image {
            media_type: format.to_mime_type().into(),
            size,
            width,
            height,
            image,
        })
    }

    #[cfg(test)]
    fn from_bytes(media_type: String, bytes: Vec<u8>) -> Result<Self, String> {
        if media_type == "application/json" {
            Ok(Self::Json(bytes))
        } else {
            Self::decode(bytes)
        }
    }
}

/// What a line can say of an image it has read at least once, after its pixels are released.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Facts {
    media_type: String,
    size: u64,
    width: u32,
    height: u32,
}

enum Presentation {
    Typed(Vec<st3_conversation_ui::Entry>),
    Raw(String),
}

fn clipped(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("[st truncated this native timeline value"),
        Value::Array(values) => values.iter().any(clipped),
        Value::Object(values) => values.values().any(clipped),
        _ => false,
    }
}

fn typed_content(
    source: &st3_client::TimelineEntry,
    block: &st3_client::TimelineBlock,
    value: &Value,
    call: Option<&st3_client::TimelineEntry>,
) -> Option<Vec<st3_conversation_ui::Entry>> {
    // Opaque references are located by the source's clipped subtree, never by
    // parsing their path. Metadata remains raw; an unambiguous typed view is
    // reattached to the original block and passed through the normal adapter.
    let payload = clipped(&block.payload)
        && !block.metadata.as_ref().is_some_and(clipped)
        && !block.view.as_ref().is_some_and(clipped);
    let view = block.view.as_ref().is_some_and(clipped)
        && !clipped(&block.payload)
        && !block.metadata.as_ref().is_some_and(clipped);
    let body = if view
        && value.get("type").and_then(Value::as_str).is_some()
        && value.get("type") == block.view.as_ref().and_then(|view| view.get("type"))
    {
        let mut body = source.body.clone();
        let blocks = match &mut body {
            TimelineBody::ToolCall(body) => &mut body.blocks,
            TimelineBody::ToolResult(body) => &mut body.blocks,
            TimelineBody::Content(body) => &mut body.blocks,
            _ => return None,
        };
        let full = blocks
            .iter_mut()
            .find(|candidate| candidate.id == block.id)?;
        full.view = Some(value.clone());
        full.continuation = None;
        body
    } else {
        match &source.body {
            TimelineBody::ToolResult(original) => {
                if let Ok(full) =
                    serde_json::from_value::<st3_client::TimelineToolResultBody>(value.clone())
                    && full.call_id == original.call_id
                {
                    TimelineBody::ToolResult(Box::new(full))
                } else if payload {
                    let mut full = original.clone();
                    full.content = value.clone();
                    TimelineBody::ToolResult(full)
                } else {
                    return None;
                }
            }
            TimelineBody::ToolCall(original) => {
                if let Ok(full) =
                    serde_json::from_value::<st3_client::TimelineToolCallBody>(value.clone())
                    && full.call_id == original.call_id
                    && full.name == original.name
                {
                    TimelineBody::ToolCall(Box::new(full))
                } else if payload {
                    let mut full = original.clone();
                    full.arguments = value.clone();
                    TimelineBody::ToolCall(full)
                } else {
                    return None;
                }
            }
            TimelineBody::Content(original) => {
                if let Ok(full) =
                    serde_json::from_value::<st3_client::TimelineContentBody>(value.clone())
                {
                    TimelineBody::Content(Box::new(full))
                } else if payload
                    && value.is_string()
                    && matches!(block.kind.as_str(), "text" | "reasoning")
                {
                    let mut full = original.clone();
                    full.text = value.as_str().map(str::to_owned);
                    TimelineBody::Content(full)
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    };
    let full = st3_client::TimelineEntry {
        id: source.id.clone(),
        sequence: source.sequence,
        revision: source.revision,
        timestamp: source.timestamp.clone(),
        role: source.role.clone(),
        is_final: source.is_final,
        body,
    };
    let mut timeline = Vec::with_capacity(2);
    if let Some(call) = call {
        timeline.push(call.clone());
    }
    timeline.push(full);
    let entries = st3_conversation_ui::adapt::conversation_full(&timeline, &BTreeMap::new());
    (!entries.is_empty()).then_some(entries)
}

pub fn has_images(blocks: &[st3_client::TimelineBlock]) -> bool {
    blocks.iter().any(|block| {
        block.kind == "image"
            || block.continuation.as_ref().is_some_and(|reference| {
                reference.reason.as_deref() == Some("on-demand")
                    || reference.media_type.starts_with("image/")
            })
            || {
                let mut refs = Vec::new();
                image_refs(&block.payload, &mut refs);
                !refs.is_empty()
            }
    })
}

/// The images a message carries, as content a reader loads from st by that message.
fn mail_refs(
    message: &st3_client::TimelineMessageBody,
) -> impl Iterator<Item = (Key, ConversationContentRef)> + '_ {
    message
        .attachments
        .iter()
        .filter(|attachment| attachment.media_type.starts_with("image/"))
        .map(move |attachment| {
            let key = image_key(&st3_conversation_ui::MailImage {
                sha256: attachment.sha256.clone(),
                message: message.message_id.clone(),
                media_type: attachment.media_type.clone(),
                name: attachment.name.clone(),
                size: attachment.size,
            });
            let reference = ConversationContentRef {
                reference: key.reference.clone(),
                media_type: attachment.media_type.clone(),
                size: Some(attachment.size),
                reason: Some(BLOB_REASON.into()),
            };
            (key, reference)
        })
}

/// An image's name as its line shows it: the last part of the name it was sent with, never a
/// path, or "image".
pub fn image_name(image: &st3_conversation_ui::MailImage) -> String {
    let leaf = image
        .name
        .as_deref()
        .and_then(|name| name.rsplit(['/', '\\']).next())
        .map(str::trim)
        .filter(|leaf| !leaf.is_empty())
        .unwrap_or("image");
    text::sanitize(leaf)
}

/// The key an image a message or an ask carries is read and kept by.
pub fn image_key(image: &st3_conversation_ui::MailImage) -> Key {
    Key {
        conversation: image.message.clone(),
        entry: image.message.clone(),
        revision: 0,
        reference: format!("blob/{}", image.sha256),
    }
}

/// An image read as a blob through the message (or ask) that carries it, not as transcript
/// content: `Key::conversation` names the carrier.
const BLOB_REASON: &str = "attachment";

#[derive(Default)]
pub struct Content {
    groups: BTreeMap<(String, String), Vec<Group>>,
    sources: BTreeMap<String, BTreeMap<String, st3_client::TimelineEntry>>,
    loaded: BTreeMap<Key, Loaded>,
    /// Explicit expand-all intent, never restored from saved expansion state.
    pub expanded_all: HashSet<String>,
    shown: BTreeSet<Key>,
    /// Explicit hiding survives redraws and scrolling, but not timeline eviction.
    hidden: BTreeSet<Key>,
    protocols: RefCell<BTreeMap<Key, (u16, u16, Encoding)>>,
    /// Encodings a draw found missing, by the size it wanted; built off the draw path.
    wanted: RefCell<BTreeMap<Key, (u16, u16)>>,
    facts: BTreeMap<Key, Facts>,
    /// The images messages carry, for their names and the line that opens them.
    mail: BTreeMap<Key, st3_conversation_ui::MailImage>,
    /// The images asks carry, read through the ask's step; kept across timeline reindexing.
    requests: BTreeMap<Key, st3_conversation_ui::MailImage>,
    pub scroll_to: RefCell<BTreeMap<String, String>>,
    pub focused: BTreeMap<String, String>,
}

/// One owner content read at a time, including all chunks of its value.
#[derive(Default)]
pub struct Reads {
    queued: std::collections::VecDeque<Key>,
    active: Option<Key>,
}

impl Reads {
    pub fn enqueue(&mut self, key: Key) {
        if self.active.as_ref() != Some(&key) && !self.queued.contains(&key) {
            self.queued.push_back(key);
        }
    }

    pub fn idle(&self) -> bool {
        self.active.is_none() && self.queued.is_empty()
    }

    pub fn next(&mut self, content: &Content) -> Option<Key> {
        if self.active.is_some() {
            return None;
        }
        while let Some(key) = self.queued.pop_front() {
            if content.is_loading(&key) {
                self.active = Some(key.clone());
                return Some(key);
            }
        }
        None
    }

    pub fn complete(&mut self, key: &Key) {
        if self.active.as_ref() == Some(key) {
            self.active = None;
        }
    }
}

fn image_refs(value: &Value, out: &mut Vec<ConversationContentRef>) {
    match value {
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("image")
                && let Some(content) = object.get("content")
                && let Ok(reference) =
                    serde_json::from_value::<ConversationContentRef>(content.clone())
            {
                out.push(reference);
            }
            if let Some(values) = object.get("image_refs").and_then(Value::as_array) {
                for value in values {
                    if let Ok(reference) = serde_json::from_value(value.clone()) {
                        out.push(reference);
                    }
                }
            }
            for (name, value) in object {
                if name != "image_refs" {
                    image_refs(value, out);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                image_refs(value, out);
            }
        }
        _ => {}
    }
}

impl Content {
    pub fn index(&mut self, timelines: &BTreeMap<String, st3_conversation_ui::Timeline>) {
        self.groups.clear();
        self.sources.clear();
        for (conversation, timeline) in timelines {
            let calls: BTreeMap<_, _> = timeline
                .items
                .iter()
                .filter_map(|entry| {
                    if let TimelineBody::ToolCall(call) = &entry.body {
                        Some((call.call_id.as_str(), entry))
                    } else {
                        None
                    }
                })
                .collect();
            for entry in &timeline.items {
                // A message's images are read from st by their message, the same way.
                if let TimelineBody::Message(message) = &entry.body {
                    let refs: Vec<_> = mail_refs(message).collect();
                    for (key, _) in &refs {
                        if let Some(attachment) = message.attachments.iter().find(|attachment| {
                            key.reference == format!("blob/{}", attachment.sha256)
                        }) {
                            self.mail.insert(key.clone(), st3_conversation_ui::MailImage {
                                sha256: attachment.sha256.clone(),
                                message: message.message_id.clone(),
                                media_type: attachment.media_type.clone(),
                                name: attachment.name.clone(),
                                size: attachment.size,
                            });
                        }
                    }
                    if !refs.is_empty() {
                        self.groups
                            .entry((conversation.clone(), message.message_id.clone()))
                            .or_default()
                            .push(Group { block_index: 0, refs });
                    }
                    continue;
                }
                let mut referenced = false;
                let image_id = format!("{}#images", entry.id);
                let (id, blocks) = match &entry.body {
                    TimelineBody::ToolCall(call) => (entry.id.as_str(), call.blocks.as_slice()),
                    TimelineBody::ToolResult(result) => (
                        calls
                            .get(result.call_id.as_str())
                            .map_or(entry.id.as_str(), |call| call.id.as_str()),
                        result.blocks.as_slice(),
                    ),
                    TimelineBody::Content(content) if has_images(&content.blocks) => {
                        (image_id.as_str(), content.blocks.as_slice())
                    }
                    TimelineBody::Content(content) => {
                        (entry.id.as_str(), content.blocks.as_slice())
                    }
                    _ => continue,
                };
                for (block_index, block) in blocks.iter().enumerate() {
                    let mut refs = Vec::new();
                    if let Some(reference) = &block.continuation {
                        refs.push(reference.clone());
                    }
                    image_refs(&block.payload, &mut refs);
                    // An image keeps its pixel continuation at block level. Clipped
                    // metadata and views carry independent JSON disclosure references.
                    for value in block.metadata.iter().chain(block.view.iter()) {
                        if let Some(value) = value.get("continuation")
                            && let Ok(reference) = serde_json::from_value(value.clone())
                        {
                            refs.push(reference);
                        }
                    }
                    let mut references = Vec::new();
                    for reference in refs {
                        let key = Key {
                            conversation: timeline
                                .session_id
                                .clone()
                                .unwrap_or_else(|| conversation.clone()),
                            entry: entry.id.clone(),
                            revision: entry.revision,
                            reference: reference.reference.clone(),
                        };
                        references.push((key.clone(), reference));
                        if let Some(Loaded::Json { images, .. }) = self.loaded.get(&key) {
                            for image in images {
                                let image_key = Key {
                                    reference: image.reference.clone(),
                                    ..key.clone()
                                };
                                references.push((image_key, image.clone()));
                            }
                        }
                    }
                    if references.is_empty() {
                        continue;
                    }
                    referenced = true;
                    self.groups
                        .entry((conversation.clone(), id.to_owned()))
                        .or_default()
                        .push(Group {
                            block_index,
                            refs: references,
                        });
                }
                // A tool's result may carry images in its content rather than in a block's payload:
                // Claude's Read of a picture does.
                if let TimelineBody::ToolResult(result) = &entry.body {
                    let mut nested = Vec::new();
                    image_refs(&result.content, &mut nested);
                    let known: BTreeSet<String> = self
                        .groups
                        .get(&(conversation.clone(), id.to_owned()))
                        .into_iter()
                        .flatten()
                        .flat_map(|group| &group.refs)
                        .map(|(key, _)| key.reference.clone())
                        .collect();
                    let refs: Vec<_> = nested
                        .into_iter()
                        .filter(|reference| !known.contains(&reference.reference))
                        .map(|reference| {
                            let key = Key {
                                conversation: timeline
                                    .session_id
                                    .clone()
                                    .unwrap_or_else(|| conversation.clone()),
                                entry: entry.id.clone(),
                                revision: entry.revision,
                                reference: reference.reference.clone(),
                            };
                            (key, reference)
                        })
                        .collect();
                    if !refs.is_empty() {
                        referenced = true;
                        self.groups
                            .entry((conversation.clone(), id.to_owned()))
                            .or_default()
                            .push(Group {
                                block_index: blocks.len(),
                                refs,
                            });
                    }
                }
                if referenced {
                    let sources = self
                        .sources
                        .entry(
                            timeline
                                .session_id
                                .clone()
                                .unwrap_or_else(|| conversation.clone()),
                        )
                        .or_default();
                    sources.insert(entry.id.clone(), entry.clone());
                    if let TimelineBody::ToolResult(result) = &entry.body
                        && let Some(call) = calls.get(result.call_id.as_str())
                    {
                        sources
                            .entry(call.id.clone())
                            .or_insert_with(|| (*call).clone());
                    }
                }
            }
        }
        let active: BTreeSet<_> = self
            .groups
            .values()
            .flatten()
            .flat_map(|group| group.refs.iter().map(|(key, _)| key.clone()))
            .chain(self.requests.keys().cloned())
            .collect();
        self.loaded.retain(|key, _| active.contains(key));
        self.facts.retain(|key, _| active.contains(key));
        self.mail.retain(|key, _| active.contains(key));
        self.wanted.get_mut().retain(|key, _| active.contains(key));
        self.shown.retain(|key| active.contains(key));
        self.hidden.retain(|key| active.contains(key));
        self.protocols
            .borrow_mut()
            .retain(|key, _| active.contains(key));
    }

    /// The images the open asks carry. Those no ask carries any more leave memory.
    pub fn index_requests(&mut self, images: Vec<st3_conversation_ui::MailImage>) {
        self.requests = images
            .into_iter()
            .map(|image| (image_key(&image), image))
            .collect();
        let known = |content: &Self, key: &Key| {
            content.requests.contains_key(key)
                || content
                    .groups
                    .values()
                    .flatten()
                    .any(|group| group.refs.iter().any(|(candidate, _)| candidate == key))
        };
        let gone: Vec<Key> = self
            .loaded
            .keys()
            .chain(self.shown.iter())
            .filter(|key| !known(self, key))
            .cloned()
            .collect();
        for key in gone {
            self.loaded.remove(&key);
            self.shown.remove(&key);
            self.hidden.remove(&key);
            self.facts.remove(&key);
            self.protocols.get_mut().remove(&key);
        }
    }

    /// A card's image lines (`Hit::Pane(PaneIntent::Image)` for an ask's image) say what st
    /// knows of the image and how to show it, and reserve rows for it once it is shown.
    pub fn expand_requests(&self, doc: &mut Doc, width: usize) {
        let mut lines: Vec<(usize, u16, Key)> = doc
            .targets
            .iter()
            .filter_map(|target| match &target.hit {
                Hit::Pane(st3_conversation_ui::PaneIntent::Image(image)) => {
                    let key = image_key(image);
                    self.requests
                        .contains_key(&key)
                        .then_some((target.line, target.column, key))
                }
                _ => None,
            })
            .collect();
        lines.sort();
        lines.dedup_by(|a, b| a.0 == b.0);
        for (line, inset, key) in lines.into_iter().rev() {
            // A card frames its lines: keep the frame on both sides of the line and its rows.
            let frame = (inset > 0)
                .then(|| doc.lines.get(line))
                .flatten()
                .filter(|row| row.spans.len() >= 2)
                .map(|row| {
                    let first = row.spans[0].clone();
                    let last = row.spans[row.spans.len() - 1].clone();
                    let inner = row.width().saturating_sub(first.width() + last.width());
                    (first, last, inner)
                });
            let inner = frame.as_ref().map_or(width, |(_, _, inner)| *inner);
            let framed = |mut spans: Vec<Span<'static>>| match &frame {
                Some((first, last, inner)) => {
                    // What does not fit ends in an ellipsis inside the frame.
                    let mut used: usize = spans.iter().map(Span::width).sum();
                    if used > *inner
                        && let Some(tail) = spans.last_mut()
                    {
                        let keep = tail.width().saturating_sub(used - inner);
                        tail.content = text::truncate(&tail.content, keep).into();
                        used = spans.iter().map(Span::width).sum();
                    }
                    let mut all = vec![first.clone()];
                    all.extend(spans);
                    all.push(Span::raw(" ".repeat(inner.saturating_sub(used))));
                    all.push(last.clone());
                    Line::from(all)
                }
                None => Line::from(spans),
            };
            let inset = if frame.is_some() { inset } else { 0 };
            let image = &self.requests[&key];
            let (media, size, pixels) = match self.facts.get(&key) {
                Some(facts) => (
                    facts.media_type.clone(),
                    facts.size,
                    format!(", {}×{}", facts.width, facts.height),
                ),
                None => (image.media_type.clone(), image.size, String::new()),
            };
            let action = self.action(&key);
            let head = format!(
                "   ▣ {} · {media} · {} KB{pixels}  ",
                image_name(image),
                size.div_ceil(1024)
            );
            let open = unicode_width::UnicodeWidthStr::width(head.as_str()) as u16;
            if let Some(row) = doc.lines.get_mut(line) {
                *row = framed(vec![
                    Span::styled(head, theme::fg(theme::TEXT)),
                    Span::styled("open", theme::fg(theme::LAVENDER)),
                    Span::styled(format!(" · {action}"), theme::dim()),
                ]);
            }
            doc.targets.retain(|target| target.line != line);
            doc.targets.push(Target {
                line,
                column: inset,
                width: open,
                hit: Hit::ContentImage(key.clone()),
            });
            doc.targets.push(Target {
                line,
                column: inset + open,
                width: 4,
                hit: Hit::Pane(st3_conversation_ui::PaneIntent::Image(image.clone())),
            });
            doc.targets.push(Target {
                line,
                column: inset + open + 4,
                width: (inner as u16).saturating_sub(open + 4),
                hit: Hit::ContentImage(key.clone()),
            });
            if self.shown.contains(&key) && matches!(self.loaded.get(&key), Some(Loaded::Image { .. })) {
                let at = line + 1;
                for target in &mut doc.targets {
                    if target.line >= at {
                        target.line += IMAGE_ROWS;
                    }
                }
                for (_, start) in &mut doc.entries {
                    if *start >= at {
                        *start += IMAGE_ROWS;
                    }
                }
                doc.lines
                    .splice(at..at, std::iter::repeat_n(framed(Vec::new()), IMAGE_ROWS));
                // The image sits inside the frame, two columns in like any other.
                doc.targets.push(Target {
                    line: at,
                    column: inset,
                    width: if frame.is_some() { (inner as u16).saturating_sub(2) } else { width as u16 },
                    hit: Hit::InlineImage(key.clone()),
                });
            }
        }
    }

    /// What a line offers for an image in its state now.
    fn action(&self, key: &Key) -> String {
        match self.loaded.get(key) {
            Some(Loaded::Loading) => "loading…".to_owned(),
            Some(Loaded::Failed(error)) => format!("{error} · load inline to retry"),
            _ if self.shown.contains(key) => "hide inline".into(),
            _ => "load inline (Ctrl+U)".into(),
        }
    }

    pub fn image_owner(&self, key: &Key) -> Option<(String, String)> {
        self.groups
            .iter()
            .find(|(_, groups)| {
                groups
                    .iter()
                    .any(|group| group.refs.iter().any(|(candidate, _)| candidate == key))
            })
            .map(|(owner, _)| owner.clone())
    }

    pub fn request_tool(&mut self, conversation: &str, entry: &str) -> Vec<Key> {
        let refs: Vec<_> = self
            .groups
            .get(&(conversation.to_owned(), entry.to_owned()))
            .into_iter()
            .flatten()
            .flat_map(|group| &group.refs)
            .filter(|(_, reference)| {
                reference.reason.as_deref() != Some("on-demand")
                    && !reference.media_type.starts_with("image/")
            })
            .map(|(key, _)| key.clone())
            .collect();
        refs.into_iter().filter(|key| self.request(key)).collect()
    }

    pub fn request_next_visible_tool(&mut self, conversation: &str, entry: &str) -> Option<Key> {
        let key = self
            .groups
            .get(&(conversation.to_owned(), entry.to_owned()))?
            .iter()
            .flat_map(|group| &group.refs)
            .find(|(key, reference)| {
                reference.reason.as_deref() != Some("on-demand")
                    && !reference.media_type.starts_with("image/")
                    && !self.loaded.contains_key(key)
            })
            .map(|(key, _)| key.clone())?;
        self.request(&key).then_some(key)
    }

    /// The caller proves the image label is visible and terminal graphics work.
    /// Never retry failures or undo a person's explicit hide on a redraw.
    pub fn request_visible_image(&mut self, key: &Key) -> bool {
        if self.hidden.contains(key)
            || self.loaded.contains_key(key)
            || !self.requests.contains_key(key)
                && !self
                    .groups
                    .values()
                    .flatten()
                    .flat_map(|group| &group.refs)
                    .any(|(candidate, _)| candidate == key)
        {
            return false;
        }
        self.shown.insert(key.clone());
        self.request(key)
    }

    pub fn is_loading(&self, key: &Key) -> bool {
        matches!(self.loaded.get(key), Some(Loaded::Loading))
    }

    /// Closing a row releases its full values, pixels and terminal encodings.
    pub fn release_tool(&mut self, conversation: &str, entry: &str) {
        let Some(groups) = self
            .groups
            .get(&(conversation.to_owned(), entry.to_owned()))
        else {
            return;
        };
        for group in groups {
            for (key, _) in &group.refs {
                if self.shown.contains(key) {
                    self.hidden.insert(key.clone());
                }
                self.loaded.remove(key);
                self.shown.remove(key);
                self.protocols.get_mut().remove(key);
            }
        }
    }

    pub fn request(&mut self, key: &Key) -> bool {
        if matches!(
            self.loaded.get(key),
            Some(Loaded::Loading | Loaded::Json { .. } | Loaded::Image { .. })
        ) {
            return false;
        }
        self.loaded.insert(key.clone(), Loaded::Loading);
        true
    }

    pub fn toggle_image(&mut self, key: &Key) -> bool {
        if matches!(self.loaded.get(key), Some(Loaded::Failed(_))) {
            self.hidden.remove(key);
            self.shown.insert(key.clone());
            return self.request(key);
        }
        if !self.shown.insert(key.clone()) {
            self.shown.remove(key);
            self.hidden.insert(key.clone());
            self.loaded.remove(key);
            self.protocols.get_mut().remove(key);
            return false;
        }
        self.hidden.remove(key);
        self.request(key)
    }

    pub fn tool_images(&self, conversation: &str, entry: &str) -> Vec<Key> {
        self.groups
            .get(&(conversation.to_owned(), entry.to_owned()))
            .into_iter()
            .flatten()
            .flat_map(|group| &group.refs)
            .filter(|(_, reference)| {
                reference.reason.as_deref() == Some("on-demand")
                    || reference.media_type.starts_with("image/")
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// A read's raw answer, decoded here: for tests, which have no worker.
    #[cfg(test)]
    pub fn complete(&mut self, key: Key, result: Result<(String, Vec<u8>), String>) {
        let result = result.and_then(|(media_type, bytes)| Fetched::from_bytes(media_type, bytes));
        self.finish(key, result);
    }

    /// A read's answer, decoded and scaled off the UI thread.
    pub fn finish(&mut self, key: Key, result: Result<Fetched, String>) {
        // An old in-flight response must never resurrect a changed revision.
        if !self.loaded.contains_key(&key) {
            return;
        }
        let loaded = match result {
            Err(error) => Loaded::Failed(error),
            Ok(Fetched::Json(bytes)) => {
                {
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(value) => {
                            let mut images = Vec::new();
                            image_refs(&value, &mut images);
                            let source = self
                                .sources
                                .get(&key.conversation)
                                .and_then(|entries| entries.get(&key.entry));
                            let group = self.groups.values().flatten().find(|group| {
                                group.refs.iter().any(|(candidate, _)| candidate == &key)
                            });
                            let entries = source.zip(group).and_then(|(source, group)| {
                                let blocks = match &source.body {
                                    TimelineBody::ToolCall(body) => &body.blocks,
                                    TimelineBody::ToolResult(body) => &body.blocks,
                                    TimelineBody::Content(body) => &body.blocks,
                                    _ => return None,
                                };
                                let call = match &source.body {
                                    TimelineBody::ToolResult(result) => self.sources.get(&key.conversation)
                                        .and_then(|sources| sources.values().find(|entry| {
                                            matches!(&entry.body, TimelineBody::ToolCall(call) if call.call_id == result.call_id)
                                        })),
                                    _ => None,
                                };
                                blocks.get(group.block_index).and_then(|block| typed_content(source, block, &value, call))
                            });
                            let presentation = entries.map_or_else(
                                || {
                                    Presentation::Raw(
                                        serde_json::to_string_pretty(&value).expect("content JSON"),
                                    )
                                },
                                Presentation::Typed,
                            );
                            Loaded::Json {
                                presentation,
                                images,
                            }
                        }
                        Err(error) => Loaded::Failed(format!("Invalid content JSON: {error}")),
                    }
                }
            }
            Ok(Fetched::Image {
                media_type,
                size,
                width,
                height,
                image,
            }) => {
                self.facts.insert(
                    key.clone(),
                    Facts {
                        media_type: media_type.clone(),
                        size,
                        width,
                        height,
                    },
                );
                Loaded::Image {
                    media_type,
                    size,
                    image: Arc::new(image),
                }
            }
        };
        self.loaded.insert(key, loaded);
    }

    pub fn decorate(
        &self,
        doc: &mut Doc,
        conversation: &str,
        expanded: &HashSet<String>,
        width: usize,
    ) {
        let entries = doc.entries.clone();
        for (index, (id, _)) in entries.iter().enumerate().rev() {
            if self.focused.get(conversation) == Some(id)
                && let Some(line) = doc.lines.get_mut(entries[index].1)
            {
                for span in &mut line.spans {
                    span.style = span.style.bg(theme::SURFACE1);
                }
            }
            let open = expanded.contains(id);
            let Some(groups) = self.groups.get(&(conversation.to_owned(), id.clone())) else {
                continue;
            };
            let at = entries
                .get(index + 1)
                .map_or(doc.lines.len(), |(_, line)| *line);
            // A message draws a line per image it carries, last; these lines take their place,
            // with what st knows of each image and a way to show it here.
            let mail_lines: Vec<usize> = doc
                .targets
                .iter()
                .filter(|target| {
                    (entries[index].1..at).contains(&target.line)
                        && matches!(
                            target.hit,
                            Hit::Pane(st3_conversation_ui::PaneIntent::Image(_))
                        )
                })
                .map(|target| target.line)
                .collect();
            let bar = mail_lines
                .first()
                .and_then(|line| doc.lines.get(*line))
                .and_then(|line| line.spans.first().cloned())
                .unwrap_or_else(|| Span::raw("  "));
            let mut extra = Doc::new();
            let mut replace = false;
            for group in groups {
                for (key, reference) in &group.refs {
                    let loaded = self.loaded.get(key);
                    let image = reference.reason.as_deref() == Some("on-demand")
                        || reference.media_type.starts_with("image/")
                        || matches!(loaded, Some(Loaded::Image { .. }));
                    if image {
                        let facts = self.facts.get(key);
                        let (media, size) = match (loaded, facts) {
                            (
                                Some(Loaded::Image {
                                    media_type, size, ..
                                }),
                                _,
                            ) => (media_type.as_str(), Some(*size)),
                            (_, Some(facts)) => (facts.media_type.as_str(), Some(facts.size)),
                            _ => (reference.media_type.as_str(), reference.size),
                        };
                        let exact =
                            size.map_or_else(String::new, |size| format!(" ({size} bytes)"));
                        let size = size.map_or_else(
                            || "size unknown".into(),
                            |size| format!("{} KB", size.div_ceil(1024)),
                        );
                        let pixels = facts.map_or_else(String::new, |facts| {
                            format!(", {}×{}", facts.width, facts.height)
                        });
                        let action = match loaded {
                            Some(Loaded::Loading) => "loading…".to_owned(),
                            Some(Loaded::Failed(error)) => {
                                format!("{error} · load inline to retry")
                            }
                            _ if self.shown.contains(key) => "hide inline".into(),
                            _ => "load inline (Ctrl+U)".into(),
                        };
                        let line = extra.lines.len();
                        if let Some(mail) = self.mail.get(key) {
                            // `▣ name · type · size  open · load inline`: the name never a path.
                            let head = format!(
                                "▣ {} · {media} · {size}{pixels}  ",
                                image_name(mail)
                            );
                            let column = unicode_width::UnicodeWidthStr::width(bar.content.as_ref());
                            let open = column + unicode_width::UnicodeWidthStr::width(head.as_str());
                            extra.targets.push(Target {
                                line,
                                column: 0,
                                width: open as u16,
                                hit: Hit::ContentImage(key.clone()),
                            });
                            extra.targets.push(Target {
                                line,
                                column: open as u16,
                                width: 4,
                                hit: Hit::Pane(st3_conversation_ui::PaneIntent::Image(mail.clone())),
                            });
                            extra.targets.push(Target {
                                line,
                                column: open as u16 + 4,
                                width: (width as u16).saturating_sub(open as u16 + 4),
                                hit: Hit::ContentImage(key.clone()),
                            });
                            extra.line(Line::from(vec![
                                bar.clone(),
                                Span::styled(head, theme::fg(theme::TEXT)),
                                Span::styled("open", theme::fg(theme::LAVENDER)),
                                Span::styled(format!(" · {action}"), theme::dim()),
                            ]));
                        } else {
                            extra.targets.push(Target {
                                line,
                                column: 0,
                                width: width as u16,
                                hit: Hit::ContentImage(key.clone()),
                            });
                            extra.line(Line::from(Span::styled(
                                format!("  [image {media}, {size}{pixels}]{exact}  {action}"),
                                theme::fg(theme::LAVENDER),
                            )));
                        }
                        if self.shown.contains(key) && matches!(loaded, Some(Loaded::Image { .. }))
                        {
                            extra.targets.push(Target {
                                line: extra.lines.len(),
                                column: 0,
                                width: width as u16,
                                hit: Hit::InlineImage(key.clone()),
                            });
                            for _ in 0..IMAGE_ROWS {
                                extra.blank();
                            }
                        }
                    } else {
                        match loaded {
                            Some(Loaded::Json { presentation, .. }) if open => match presentation {
                                Presentation::Typed(entries) => {
                                    replace = true;
                                    let expanded =
                                        entries.iter().map(|entry| entry.id.clone()).collect();
                                    extra.append(
                                        super::conversation::Cache::default().render(
                                            entries,
                                            width,
                                            &expanded,
                                            "",
                                            st3_conversation_ui::Density::Full,
                                        ),
                                        0,
                                    );
                                }
                                Presentation::Raw(text) => {
                                    extra.line(Line::from(Span::styled(
                                        "  Full referenced content (JSON)",
                                        theme::dim(),
                                    )));
                                    extra.lines(text::wrap(
                                        &[text::run(text::sanitize(text), theme::fg(theme::TEXT))],
                                        width,
                                        &[text::run("  ", theme::dim())],
                                        &[text::run("  ", theme::dim())],
                                        None,
                                    ));
                                }
                            },
                            Some(Loaded::Loading) => extra.line(Line::from(Span::styled(
                                "  Loading full output…",
                                theme::dim(),
                            ))),
                            Some(Loaded::Json { .. }) => {}
                            failed => {
                                let label = match failed {
                                    Some(Loaded::Failed(error)) => {
                                        format!("  {error} · retry full output")
                                    }
                                    _ => "  load full output · Ctrl+Enter".into(),
                                };
                                extra.targets.push(Target {
                                    line: extra.lines.len(),
                                    column: 0,
                                    width: width as u16,
                                    hit: Hit::ContentOutput(key.clone()),
                                });
                                extra.line(Line::from(Span::styled(
                                    label,
                                    theme::fg(theme::LAVENDER),
                                )));
                            }
                        }
                    }
                }
            }
            if open
                && groups
                    .iter()
                    .flat_map(|group| &group.refs)
                    .any(|(key, _)| self.loaded.contains_key(key))
            {
                extra.targets.push(Target {
                    line: extra.lines.len(),
                    column: 0,
                    width: width as u16,
                    hit: Hit::ToggleTool(id.clone()),
                });
                extra.line(Line::from(Span::styled(
                    "  collapse full content · Ctrl+Enter",
                    theme::dim(),
                )));
            }
            // What the new lines take the place of: the whole entry for typed content, a
            // message's own image lines, or nothing (they follow the entry).
            let (start, at) = match (replace, mail_lines.iter().min(), mail_lines.iter().max()) {
                (true, _, _) => (entries[index].1, at),
                (false, Some(first), Some(last)) => (*first, last + 1),
                _ => (at, at),
            };
            let shift = extra.lines.len() as isize - (at - start) as isize;
            if start < at {
                doc.targets
                    .retain(|target| target.line < start || target.line >= at);
            }
            for target in &mut doc.targets {
                if target.line >= at {
                    target.line = target.line.saturating_add_signed(shift);
                }
            }
            for (_, line) in &mut doc.entries {
                if *line >= at {
                    *line = line.saturating_add_signed(shift);
                }
            }
            for (_, range) in &mut doc.messages {
                if range.start >= at {
                    range.start = range.start.saturating_add_signed(shift);
                }
                if range.end >= at {
                    range.end = range.end.saturating_add_signed(shift);
                }
            }
            doc.lines.splice(start..at, extra.lines);
            doc.targets
                .extend(extra.targets.into_iter().map(|mut target| {
                    target.line += start;
                    target
                }));
        }
    }

    /// Draw an image whose encoding is ready, at its full size: `hidden_above` of its rows are
    /// scrolled off and `visible` is what remains on screen. An image without an encoding for
    /// this width asks for one ([`Content::protocol_jobs`]) and draws nothing yet. Half blocks
    /// may be cut at a pane's edge; terminal graphics are drawn only whole.
    pub fn draw_image(
        &self,
        key: &Key,
        width: u16,
        hidden_above: u16,
        visible: Rect,
        buf: &mut Buffer,
    ) {
        if !self.shown.contains(key) || !matches!(self.loaded.get(key), Some(Loaded::Image { .. }))
        {
            return;
        }
        let rows = IMAGE_ROWS as u16;
        let protocols = self.protocols.borrow();
        let Some((_, _, protocol)) = protocols
            .get(key)
            .filter(|(w, h, _)| *w == width && *h == rows)
        else {
            self.wanted.borrow_mut().insert(key.clone(), (width, rows));
            return;
        };
        // Only the rows on screen: an image cut by the top or bottom edge draws its visible part
        // in every protocol (kitty's placeholders name their row, sixel drops bands, iTerm2 is
        // encoded row by row), never a blank where it was.
        use ratatui::widgets::Widget as _;
        let position = ratatui_image::sliced::SignedPosition::from((0, -(hidden_above as i16)));
        ratatui_image::sliced::SlicedImage::new(protocol, position)
            .render(Rect { width: width.min(visible.width), ..visible }, buf);
    }

    /// The encodings draws asked for, with the image each is built from: built on a worker,
    /// never on the draw path, and handed back with [`Content::install_protocol`].
    pub fn protocol_jobs(&self) -> Vec<(Key, u16, u16, Arc<image::DynamicImage>)> {
        let mut wanted = self.wanted.borrow_mut();
        let jobs = wanted
            .iter()
            .filter_map(|(key, (width, height))| match self.loaded.get(key) {
                Some(Loaded::Image { image, .. }) => {
                    Some((key.clone(), *width, *height, image.clone()))
                }
                _ => None,
            })
            .collect();
        wanted.clear();
        jobs
    }

    /// Build the encodings draws asked for here, as the worker does: for tests.
    #[cfg(test)]
    pub fn build_wanted(&mut self, picker: &ratatui_image::picker::Picker) {
        for (key, width, height, image) in self.protocol_jobs() {
            if let Some(protocol) = encode(picker, &image, width, height) {
                self.install_protocol(key, width, height, protocol);
            }
        }
    }

    /// One encoding per image, the last one built for the size it is drawn at.
    pub fn install_protocol(
        &mut self,
        key: Key,
        width: u16,
        height: u16,
        protocol: Encoding,
    ) {
        if self.shown.contains(&key) && matches!(self.loaded.get(&key), Some(Loaded::Image { .. }))
        {
            self.protocols
                .get_mut()
                .insert(key, (width, height, protocol));
        }
    }

    /// Release the pixels and encodings of every image not in `near`, the images on or close to
    /// a viewport. What each is (type, size, dimensions) stays, and one the person showed is read
    /// again when it comes back into view.
    pub fn release_far(&mut self, near: &BTreeSet<Key>) {
        let far: Vec<Key> = self
            .loaded
            .iter()
            .filter(|(key, loaded)| matches!(loaded, Loaded::Image { .. }) && !near.contains(*key))
            .map(|(key, _)| key.clone())
            .collect();
        for key in far {
            self.loaded.remove(&key);
            self.protocols.get_mut().remove(&key);
            self.wanted.get_mut().remove(&key);
        }
    }

    /// An image the person showed whose pixels were released: read it again once visible.
    pub fn shown_released(&self, key: &Key) -> bool {
        self.shown.contains(key) && !self.loaded.contains_key(key)
    }
}

/// Read one piece of content: an image a message or an ask carries by its blob reference, or a
/// transcript value in chunks. An image is decoded and scaled on a blocking worker.
pub async fn fetch(client: &Client, key: &Key) -> Result<Fetched, String> {
    let (media_type, bytes) = if let Some(sha256) = key.reference.strip_prefix("blob/") {
        let bytes = client
            .blob(sha256, Some(key.conversation.as_str()))
            .await
            .map_err(|error| error.plain())?;
        (String::new(), bytes)
    } else {
        let mut bytes = Vec::new();
        let mut offset = 0;
        let mut identity = None;
        loop {
            let chunk = client
                .conversation_content_chunk(&key.conversation, &key.reference, offset)
                .await
                .map_err(|error| error.plain())?
                .value;
            append_chunk(&mut bytes, &mut identity, &key.reference, offset, &chunk)?;
            match chunk.next_offset {
                Some(next) => offset = next,
                None => break (chunk.media_type, bytes),
            }
        }
    };
    if media_type == "application/json" {
        return Ok(Fetched::Json(bytes));
    }
    tokio::task::spawn_blocking(move || Fetched::decode(bytes))
        .await
        .map_err(|error| error.to_string())?
}

fn append_chunk(
    bytes: &mut Vec<u8>,
    identity: &mut Option<(String, u64)>,
    reference: &str,
    offset: u64,
    chunk: &st3_client::ConversationContentChunk,
) -> Result<(), String> {
    if chunk.kind != "conversation-content-chunk"
        || chunk.reference != reference
        || chunk.offset != offset
        || offset != bytes.len() as u64
    {
        return Err("Content chunk identity or offset changed".into());
    }
    if identity
        .as_ref()
        .is_some_and(|(media, size)| media != &chunk.media_type || *size != chunk.size)
    {
        return Err("Content changed while loading".into());
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(&chunk.data)
        .map_err(|error| error.to_string())?;
    let end = offset
        .checked_add(data.len() as u64)
        .ok_or("Content size overflow")?;
    if end > chunk.size
        || match chunk.next_offset {
            Some(next) => next != end || next <= offset || next >= chunk.size,
            None => end != chunk.size,
        }
    {
        return Err("Content chunk is incomplete or non-progressing".into());
    }
    *identity = Some((chunk.media_type.clone(), chunk.size));
    bytes.extend_from_slice(&data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_assemble_the_entire_original_json_from_zero() {
        let mut bytes = Vec::new();
        let mut identity = None;
        for (offset, data, next) in [(0, "{\"a\":", Some(5)), (5, "123}", None)] {
            let chunk = st3_client::ConversationContentChunk {
                kind: "conversation-content-chunk".into(),
                reference: "ref".into(),
                media_type: "application/json".into(),
                offset,
                size: 9,
                data: base64::engine::general_purpose::STANDARD.encode(data),
                next_offset: next,
            };
            append_chunk(&mut bytes, &mut identity, "ref", offset, &chunk).unwrap();
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            serde_json::json!({"a":123})
        );
    }

    #[test]
    fn chunks_reject_changed_identity_and_non_progressing_offsets() {
        let mut chunk = st3_client::ConversationContentChunk {
            kind: "conversation-content-chunk".into(),
            reference: "other".into(),
            media_type: "application/json".into(),
            offset: 0,
            size: 2,
            data: "e30=".into(),
            next_offset: None,
        };
        assert!(append_chunk(&mut Vec::new(), &mut None, "ref", 0, &chunk).is_err());
        chunk.reference = "ref".into();
        chunk.next_offset = Some(0);
        assert!(append_chunk(&mut Vec::new(), &mut None, "ref", 0, &chunk).is_err());
    }
    fn fixture(
        image: bool,
    ) -> (
        Content,
        BTreeMap<String, st3_conversation_ui::Timeline>,
        Vec<super::super::view::Entry>,
    ) {
        let source = if image {
            serde_json::json!({"id":"image-entry","sequence":1,"revision":1,"timestamp":"2026-10-08T10:00:00Z",
            "role":"assistant","type":"content","final":true,"body":{"media_type":"image/png","blocks":[{
                "id":"image-block","kind":"image","source_type":"native","payload":{},
                "continuation":{"ref":"image-ref","media_type":"application/octet-stream","reason":"on-demand"}
            }]}})
        } else {
            serde_json::json!({"id":"call","sequence":1,"revision":1,"timestamp":"2026-10-08T10:00:00Z",
                "role":"assistant","type":"tool_call","final":true,"body":{"call_id":"c","name":"exec","arguments":{},
                    "blocks":[{"id":"block","kind":"tool_call","source_type":"native","payload":{"preview":"clipped"},
                    "metadata":{"source":"native"},"view":{"type":"exec"},
                    "continuation":{"ref":"full","media_type":"application/json","reason":"size-limit"}}]}})
        };
        let items = vec![serde_json::from_value(source).unwrap()];
        let entries = super::super::adapt::conversation(&items, &BTreeMap::new());
        let timelines = BTreeMap::from([(
            "agent/example".into(),
            st3_conversation_ui::Timeline {
                items,
                session_id: Some("session/example".into()),
                ..Default::default()
            },
        )]);
        let mut content = Content::default();
        content.index(&timelines);
        (content, timelines, entries)
    }

    fn render(content: &Content, entries: &[super::super::view::Entry], open: bool) -> Doc {
        render_as(content, entries, open, st3_conversation_ui::Density::Full)
    }

    fn render_as(
        content: &Content,
        entries: &[super::super::view::Entry],
        open: bool,
        density: st3_conversation_ui::Density,
    ) -> Doc {
        let expanded = if open {
            entries.iter().map(|entry| entry.id.clone()).collect()
        } else {
            HashSet::new()
        };
        let mut doc = super::super::conversation::Cache::default().render(
            entries,
            120,
            &expanded,
            "*",
            density,
        );
        content.decorate(&mut doc, "agent/example", &expanded, 120);
        doc
    }

    fn words(doc: &Doc) -> String {
        doc.lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Each line's text whole, one line per row.
    fn whole(doc: &Doc) -> String {
        doc.lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn simple_run(image: bool) -> (Content, Vec<super::super::view::Entry>) {
        let mut items = Vec::new();
        for index in 0..3 {
            let call_id = format!("c{index}");
            let clipped = "preview\n[st truncated this native timeline value: size limit; 20000 bytes]";
            let reference = if image && index == 1 {
                serde_json::json!({"ref":"image-ref","media_type":"image/png","reason":"on-demand"})
            } else {
                serde_json::json!({"ref":format!("full-{index}"),"media_type":"application/json","reason":"size-limit"})
            };
            for (id, sequence, body) in [
                (format!("call-{index}"), index * 2, serde_json::json!({
                    "type":"tool_call","body":{"call_id":call_id,"name":"bash",
                        "arguments":{"command":format!("printf row-{index}")},"blocks":[]}
                })),
                (format!("result-{index}"), index * 2 + 1, serde_json::json!({
                    "type":"tool_result","body":{"call_id":call_id,"status":"success",
                        "media_type":"text/plain","content":clipped,
                        "blocks":[{"id":format!("block-{index}"),"kind":"tool_output","source_type":"native",
                            "payload":clipped,"view":{"type":"bash","exit_code":0},"continuation":reference}]}
                })),
            ] {
                let mut entry = serde_json::json!({
                    "id":id,"sequence":sequence,"revision":1,"timestamp":"2026-10-08T10:00:00Z",
                    "role":"assistant","final":true,
                });
                entry.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
                items.push(serde_json::from_value(entry).unwrap());
            }
        }
        let entries = super::super::adapt::conversation(&items, &BTreeMap::new());
        assert_eq!(entries.len(), 3);
        let timelines = BTreeMap::from([(
            "agent/example".into(),
            st3_conversation_ui::Timeline {
                items,
                session_id: Some("session/example".into()),
                ..Default::default()
            },
        )]);
        let mut content = Content::default();
        content.index(&timelines);
        (content, entries)
    }

    fn render_simple(content: &Content, entries: &[super::super::view::Entry], open: bool) -> Doc {
        let mut expanded = HashSet::new();
        if open {
            expanded.extend(entries.iter().map(|entry| entry.id.clone()));
            expanded.insert(st3_conversation_ui::bundle_id(&entries[0].id));
        }
        let mut doc = super::super::conversation::Cache::default().render(
            entries, 120, &expanded, "*", st3_conversation_ui::Density::Simple,
        );
        content.decorate(&mut doc, "agent/example", &expanded, 120);
        doc
    }

    fn assert_simple_content_row(doc: &Doc, content: &str, index: usize) {
        let row = |id: &str| doc.entries.iter().find(|(entry, _)| entry == id).unwrap().1;
        let at = doc.lines.iter().position(|line| {
            line.spans.iter().any(|span| span.content.contains(content))
        }).unwrap();
        assert!(at >= row(&format!("call-{index}")));
        if index < 2 {
            assert!(at < row(&format!("call-{}", index + 1)));
        }
        let shown = words(doc);
        assert!(shown.contains("3 tool calls"), "{shown}");
        for index in 0..3 {
            assert!(shown.contains(&format!("printf row-{index}")), "{shown}");
        }
    }

    #[test]
    fn simple_loaded_json_stays_under_its_call_in_a_multi_call_run() {
        let (mut content, entries) = simple_run(false);
        let key = content.request_tool("agent/example", "call-1").pop().unwrap();
        content.complete(key, Ok(("application/json".into(), br#""loaded middle output""#.to_vec())));
        let doc = render_simple(&content, &entries, true);
        assert_simple_content_row(&doc, "loaded middle output", 1);
        let folded = words(&render_simple(&content, &entries, false));
        assert!(folded.contains("3 tool calls"));
        assert!(!folded.contains("loaded middle output"));
        assert!(!folded.contains("load full output"));
    }

    #[test]
    fn simple_loaded_image_stays_under_its_call_in_a_multi_call_run() {
        let (mut content, entries) = simple_run(true);
        let key = content.tool_images("agent/example", "call-1").pop().unwrap();
        assert!(content.toggle_image(&key));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0])))
            .write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        content.complete(key.clone(), Ok(("image/png".into(), bytes.into_inner())));
        let doc = render_simple(&content, &entries, true);
        assert_simple_content_row(&doc, "[image image/png", 1);
        let image = doc.targets.iter().find(|target| {
            matches!(&target.hit, Hit::InlineImage(image) if image == &key)
        }).unwrap();
        let next = doc.entries.iter().find(|(id, _)| id == "call-2").unwrap().1;
        assert!(image.line < next);
    }

    #[test]
    fn simple_last_loaded_typed_result_does_not_replace_the_run() {
        let (mut content, entries) = simple_run(false);
        let key = content.request_tool("agent/example", "call-2").pop().unwrap();
        content.complete(key.clone(), Ok(("application/json".into(), br#""loaded final output""#.to_vec())));
        assert!(matches!(content.loaded.get(&key), Some(Loaded::Json {
            presentation: Presentation::Typed(_), ..
        })));
        assert_simple_content_row(&render_simple(&content, &entries, true), "loaded final output", 2);
    }

    #[test]
    fn ordinary_task_blocks_preserve_the_baseline_and_never_offer_full_loading() {
        let (mut content, mut timelines, _) = fixture(false);
        let timeline = timelines.get_mut("agent/example").unwrap();
        let TimelineBody::ToolCall(call) = &mut timeline.items[0].body else {
            panic!("tool call")
        };
        call.name = "task".into();
        call.blocks[0].continuation = None;
        call.blocks[0].view = Some(
            serde_json::json!({"type":"task","agents":[{"id":"child","description":"a child task"}]}),
        );
        let mut bash = timeline.items[0].clone();
        bash.id = "bash-call".into();
        bash.body = TimelineBody::ToolCall(Box::new(st3_client::TimelineToolCallBody {
            call_id: "bash-call".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command":"printf E2E-BASH-ROW"}),
            blocks: vec![],
        }));
        timeline.items.push(bash);
        let entries = super::super::adapt::conversation(&timeline.items, &BTreeMap::new());
        content.index(&timelines);
        for open in [false, true] {
            let expanded = if open {
                entries.iter().map(|entry| entry.id.clone()).collect()
            } else {
                HashSet::new()
            };
            let baseline = super::super::conversation::Cache::default().render(
                &entries,
                120,
                &expanded,
                "*",
                st3_conversation_ui::Density::Full,
            );
            let shown = words(&render(&content, &entries, open));
            assert_eq!(shown, words(&baseline));
            assert!(shown.contains("$ printf E2E-BASH-ROW"));
            assert!(!shown.contains("collapse full content"));
            assert!(!shown.contains("load full output"));
        }
        assert!(content.loaded.is_empty());
        assert!(content.groups.is_empty());
        assert!(content.request_tool("agent/example", "call").is_empty());
    }

    #[test]
    fn persisted_expansion_does_not_fetch_or_show_full_content() {
        let (content, _, entries) = fixture(false);
        let shown = words(&render(&content, &entries, true));
        assert!(
            content.expanded_all.is_empty(),
            "restored rows have no loading intent"
        );
        assert!(shown.contains("load full output"));
        assert!(!shown.contains("payload / metadata / view"));
        assert!(!shown.contains("collapse full content"));
        assert!(!shown.contains("Loading full"));
        assert!(content.loaded.is_empty());
    }

    #[test]
    fn fetched_tool_body_and_payload_use_typed_output_without_the_preview_line_limit() {
        let output = (0..600)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        for whole_body in [false, true] {
            let (mut content, mut timelines, _) = fixture(false);
            let timeline = timelines.get_mut("agent/example").unwrap();
            let mut result = timeline.items[0].clone();
            result.id = "result".into();
            result.body = serde_json::from_value::<st3_client::TimelineBody>(serde_json::json!({
                "type":"tool_result", "body":{
                    "call_id":"c","status":"success","media_type":"text/plain",
                    "content":"prefix\n[st truncated this native timeline value: size limit; 20000 bytes]",
                    "blocks":[{"id":"result-block","kind":"tool_output","source_type":"native",
                        "payload":"prefix\n[st truncated this native timeline value: size limit; 20000 bytes]",
                        "view":{"type":"bash","exit_code":0},
                        "continuation":{"ref":"result-ref","media_type":"application/json","reason":"size-limit"}}]
                }
            })).unwrap();
            let TimelineBody::ToolCall(call) = &mut timeline.items[0].body else {
                panic!("tool call")
            };
            call.name = "bash".into();
            call.arguments = serde_json::json!({"command":"printf E2E-BASH-ROW"});
            call.blocks.clear();
            timeline.items.push(result);
            let entries = super::super::adapt::conversation(&timeline.items, &BTreeMap::new());
            content.index(&timelines);
            let key = content.request_tool("agent/example", "call").pop().unwrap();
            let value = if whole_body {
                serde_json::json!({"call_id":"c","status":"success","media_type":"text/plain","content":output,
                    "blocks":[{"id":"result-block","kind":"tool_output","source_type":"native","payload":{"body_ref":true},"view":{"type":"bash","exit_code":0}}]})
            } else {
                serde_json::json!(output)
            };
            content.complete(
                key.clone(),
                Ok((
                    "application/json".into(),
                    serde_json::to_vec(&value).unwrap(),
                )),
            );
            assert!(matches!(
                content.loaded.get(&key),
                Some(Loaded::Json {
                    presentation: Presentation::Typed(_),
                    ..
                })
            ));
            let shown = words(&render(&content, &entries, true));
            assert!(shown.contains("$ printf E2E-BASH-ROW"));
            assert!(shown.contains("line-599"));
            assert!(!shown.contains("st truncated"));
            assert!(!shown.contains("Full referenced content (JSON)"));
            assert!(!shown.contains("\"content\""));
        }
    }

    #[test]
    fn expanded_content_preserves_complete_payload_metadata_and_view_without_guessing_location() {
        let (mut content, _, entries) = fixture(false);
        let key = content.request_tool("agent/example", "call").pop().unwrap();
        assert_eq!(key.conversation, "session/example");
        let lines = (0..600)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        content.complete(key, Ok(("application/json".into(), serde_json::to_vec(&serde_json::json!({
            "metadata":{"entire":lines}, "view":{"type":"exec","complete":true}, "payload":{"tail":"last-byte"}
        })).unwrap())));
        let expanded = words(&render(&content, &entries, true));
        assert!(expanded.contains("line-599"));
        assert!(expanded.contains("last-byte"));
        assert!(expanded.contains("complete"));
        assert!(!words(&render(&content, &entries, false)).contains("last-byte"));
        assert!(content.request_tool("agent/example", "call").is_empty());
    }

    #[test]
    fn revision_change_evicts_content_and_ignores_late_responses() {
        let (mut content, mut timelines, _) = fixture(false);
        let old = content.request_tool("agent/example", "call").pop().unwrap();
        timelines.get_mut("agent/example").unwrap().items[0].revision += 1;
        content.index(&timelines);
        content.complete(old.clone(), Ok(("application/json".into(), b"{}".to_vec())));
        assert!(!content.loaded.contains_key(&old));
        let new = content.request_tool("agent/example", "call").pop().unwrap();
        assert_eq!(new.revision, 2);
        assert_ne!(new, old);
    }

    #[test]
    fn standalone_images_are_explicit_and_unknown_until_loaded_then_render_from_memory() {
        let (mut content, _, entries) = fixture(true);
        assert_eq!(entries[0].id, "image-entry#images");
        assert!(
            content
                .request_tool("agent/example", &entries[0].id)
                .is_empty()
        );
        let before = words(&render(&content, &entries, true));
        assert!(before.contains("[image application/octet-stream, size unknown]"));
        assert!(before.contains("load inline"));
        let key = content
            .tool_images("agent/example", &entries[0].id)
            .pop()
            .unwrap();
        assert!(content.toggle_image(&key));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0])))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let size = bytes.get_ref().len() as u64;
        content.complete(
            key.clone(),
            Ok(("application/octet-stream".into(), bytes.into_inner())),
        );
        let after = render(&content, &entries, true);
        assert!(words(&after).contains(&format!("[image image/png, {} KB, 2×2]", size.div_ceil(1024))));
        assert!(
            after
                .targets
                .iter()
                .any(|target| matches!(target.hit, Hit::InlineImage(_)))
        );
        let area = Rect::new(0, 0, 16, 12);
        let mut buffer = Buffer::empty(area);
        // The first draw only asks for an encoding; the draw after it is built shows it.
        content.draw_image(&key, area.width, 0, area, &mut buffer);
        assert!(buffer.content.iter().all(|cell| cell.symbol() == " "));
        content.build_wanted(&ratatui_image::picker::Picker::halfblocks());
        content.draw_image(&key, area.width, 0, area, &mut buffer);
        assert_eq!(content.protocols.borrow().len(), 1);
        assert!(buffer.content.iter().any(|cell| cell.symbol() != " "));
        assert!(!content.toggle_image(&key));
        assert!(
            !render(&content, &entries, true)
                .targets
                .iter()
                .any(|target| matches!(target.hit, Hit::InlineImage(_)))
        );
    }

    #[test]
    fn image_metadata_and_view_refs_are_indexed_as_json_disclosures_beside_pixels() {
        let (mut content, mut timelines, entries) = fixture(true);
        let timeline = timelines.get_mut("agent/example").unwrap();
        let TimelineBody::Content(body) = &mut timeline.items[0].body else {
            panic!("expected image content");
        };
        let block = &mut body.blocks[0];
        block.metadata = Some(serde_json::json!({
            "model":"[st truncated this native timeline value]",
            "continuation":{"ref":"metadata-ref","media_type":"application/json","reason":"size-limit"}
        }));
        block.view = Some(serde_json::json!({
            "detail":"[st truncated this native timeline value]",
            "continuation":{"ref":"view-ref","media_type":"application/json","reason":"size-limit"}
        }));
        content.index(&timelines);
        let image_keys = content.tool_images("agent/example", &entries[0].id);
        assert_eq!(image_keys.len(), 1);
        let pixel = &image_keys[0];
        assert_eq!(pixel.reference, "image-ref");
        let json = content.request_tool("agent/example", &entries[0].id);
        assert_eq!(
            json.iter()
                .map(|key| key.reference.as_str())
                .collect::<Vec<_>>(),
            ["metadata-ref", "view-ref"]
        );
        assert!(!content.is_loading(pixel));
        for key in json {
            let full = if key.reference == "metadata-ref" {
                serde_json::json!({"model":"complete synthetic model"})
            } else {
                serde_json::json!({"detail":"complete synthetic view"})
            };
            content.complete(
                key,
                Ok((
                    "application/json".into(),
                    serde_json::to_vec(&full).unwrap(),
                )),
            );
        }
        let shown = words(&render(&content, &entries, true));
        assert!(shown.contains("complete synthetic model"));
        assert!(shown.contains("complete synthetic view"));
        assert!(shown.contains("Full referenced content"));
        assert!(shown.contains("[image application/octet-stream, size unknown]"));
        assert_eq!(
            content.tool_images("agent/example", &entries[0].id),
            image_keys
        );
        assert!(content.toggle_image(pixel));
        assert!(content.is_loading(pixel));
    }

    #[test]
    fn repeated_image_load_hide_and_resize_release_bytes_and_keep_one_protocol() {
        let (mut content, _, entries) = fixture(true);
        let key = content
            .tool_images("agent/example", &entries[0].id)
            .pop()
            .unwrap();
        let picker = ratatui_image::picker::Picker::halfblocks();
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0])))
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        for _ in 0..8 {
            assert!(content.toggle_image(&key));
            content.complete(
                key.clone(),
                Ok(("application/octet-stream".into(), encoded.get_ref().clone())),
            );
            assert_eq!(content.loaded.len(), 1);
            let Some(Loaded::Image { image, .. }) = content.loaded.get(&key) else {
                panic!("loaded pixels")
            };
            assert_eq!(image.as_bytes().len(), 12);
            for width in 8..24 {
                let area = Rect::new(0, 0, width, 12);
                content.draw_image(&key, width, 0, area, &mut Buffer::empty(area));
                content.build_wanted(&picker);
                let protocols = content.protocols.borrow();
                assert_eq!(protocols.len(), 1);
                assert_eq!(protocols[&key].0, width);
            }
            assert!(!content.toggle_image(&key));
            assert!(
                content.loaded.is_empty(),
                "hidden pixels must not remain retained"
            );
            assert!(content.shown.is_empty());
            assert!(content.protocols.borrow().is_empty());
        }
    }

    #[test]
    fn tool_collapse_releases_full_json_and_ignores_late_responses() {
        let (mut content, _, _) = fixture(false);
        for _ in 0..8 {
            let key = content.request_tool("agent/example", "call").pop().unwrap();
            content.complete(
                key.clone(),
                Ok((
                    "application/json".into(),
                    serde_json::to_vec(&serde_json::json!({"output":"x".repeat(16384)})).unwrap(),
                )),
            );
            assert_eq!(content.loaded.len(), 1);
            content.release_tool("agent/example", "call");
            assert!(content.loaded.is_empty());
            assert!(content.protocols.borrow().is_empty());
            content.complete(key, Ok(("application/json".into(), b"{}".to_vec())));
            assert!(content.loaded.is_empty());
        }
        let key = content.request_tool("agent/example", "call").pop().unwrap();
        content.release_tool("agent/example", "call");
        content.complete(key, Ok(("application/json".into(), b"{}".to_vec())));
        assert!(
            content.loaded.is_empty(),
            "closing an in-flight request must not retain its result"
        );
    }

    #[test]
    fn fetched_json_discovers_nested_image_refs_without_loading_them() {
        let (mut content, timelines, _) = fixture(false);
        let key = content.request_tool("agent/example", "call").pop().unwrap();
        content.complete(key, Ok(("application/json".into(), serde_json::to_vec(&serde_json::json!({
            "payload":{"type":"image","content":{"ref":"nested-image","media_type":"application/octet-stream","reason":"on-demand"}}
        })).unwrap())));
        content.index(&timelines);
        let images = content.tool_images("agent/example", "call");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].reference, "nested-image");
        assert!(!content.loaded.contains_key(&images[0]));
    }

    #[test]
    fn metadata_subtree_content_is_displayed_without_replacing_payload() {
        let (mut content, _, entries) = fixture(false);
        let key = content.request_tool("agent/example", "call").pop().unwrap();
        content.complete(
            key,
            Ok((
                "application/json".into(),
                br#"{"metadata-tail":"all metadata"}"#.to_vec(),
            )),
        );
        let text = words(&render(&content, &entries, true));
        assert!(text.contains("all metadata"));
        assert!(text.contains("exec"));
        assert!(text.contains("Full referenced content"));
    }

    #[test]
    fn identical_native_entry_ids_and_refs_are_isolated_by_conversation() {
        let (mut content, mut timelines, _) = fixture(false);
        let first = content.request_tool("agent/example", "call").pop().unwrap();
        let second = st3_conversation_ui::Timeline {
            items: timelines["agent/example"].items.clone(),
            session_id: Some("session/second".into()),
            ..Default::default()
        };
        timelines.insert("agent/second".into(), second);
        content.index(&timelines);
        let second = content.request_tool("agent/second", "call").pop().unwrap();
        assert_ne!(first, second);
        content.complete(
            first.clone(),
            Ok(("application/json".into(), b"{}".to_vec())),
        );
        assert!(matches!(content.loaded.get(&second), Some(Loaded::Loading)));
    }
    #[test]
    fn visible_images_auto_load_once_and_explicit_hide_survives_reindex() {
        let (mut content, mut timelines, entries) = fixture(true);
        let key = content
            .tool_images("agent/example", &entries[0].id)
            .pop()
            .unwrap();
        assert!(content.request_visible_image(&key));
        assert!(!content.request_visible_image(&key));
        assert!(
            !content.toggle_image(&key),
            "Ctrl+U hides even an in-flight image"
        );
        content.complete(key.clone(), Err("late result".into()));
        content.index(&timelines);
        assert!(
            !content.request_visible_image(&key),
            "redraw cannot reverse hiding"
        );
        assert!(
            content.toggle_image(&key),
            "only an explicit toggle shows it again"
        );
        content.complete(key.clone(), Err("synthetic owner unavailable".into()));
        assert!(
            !content.request_visible_image(&key),
            "failures require explicit retry"
        );
        assert!(content.toggle_image(&key));
        timelines.clear();
        content.index(&timelines);
        assert!(content.loaded.is_empty());
        assert!(content.shown.is_empty());
        assert!(content.hidden.is_empty());
        assert!(
            !content.request_visible_image(&key),
            "a stale frame cannot resurrect an evicted image"
        );
    }

    #[test]
    fn auto_loaded_image_renders_without_tool_expansion_and_eviction_releases_pixels() {
        let (mut content, mut timelines, entries) = fixture(true);
        let key = content
            .tool_images("agent/example", &entries[0].id)
            .pop()
            .unwrap();
        assert!(content.request_visible_image(&key));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([0, 128, 255]),
        ))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
        content.complete(key.clone(), Ok(("image/png".into(), bytes.into_inner())));
        assert!(
            render(&content, &entries, false)
                .targets
                .iter()
                .any(|target| matches!(target.hit, Hit::InlineImage(_)))
        );
        let area = Rect::new(0, 0, 16, 12);
        content.draw_image(&key, area.width, 0, area, &mut Buffer::empty(area));
        content.build_wanted(&ratatui_image::picker::Picker::halfblocks());
        assert_eq!(content.protocols.borrow().len(), 1);
        timelines.clear();
        content.index(&timelines);
        assert!(content.loaded.is_empty());
        assert!(content.protocols.borrow().is_empty());
    }

    #[test]
    fn fetched_view_subtrees_reproject_write_task_eval_and_compaction_without_raw_json() {
        let marker = "[st truncated this native timeline value]";
        for (kind, field) in [
            ("write", "content"),
            ("task", "context"),
            ("eval", "code"),
            ("compaction", "summary"),
        ] {
            let mut view = serde_json::json!({"type":kind, "path":"synthetic.txt", "language":"typescript", "method":"automatic"});
            view[field] = Value::String(marker.into());
            let block = serde_json::json!({
                "id":"view-block","kind":if kind == "compaction" { "compaction" } else { "tool_call" },
                "source_type":"native","payload":{},"view":view,
                "continuation":{"ref":"opaque-view-reference","media_type":"application/json","reason":"size-limit"}
            });
            let body = if kind == "compaction" {
                serde_json::json!({"media_type":"text/plain","text":"compaction preview","blocks":[block]})
            } else {
                serde_json::json!({"call_id":"view-call","name":kind,"arguments":{},"blocks":[block]})
            };
            let source: st3_client::TimelineEntry = serde_json::from_value(serde_json::json!({
                "id":"view-entry","sequence":1,"revision":1,"timestamp":"2026-10-08T10:00:00Z",
                "role":"assistant","type":if kind == "compaction" { "content" } else { "tool_call" },
                "final":true,"body":body
            })).unwrap();
            let items = vec![source];
            let entries = super::super::adapt::conversation(&items, &BTreeMap::new());
            let mut content = Content::default();
            content.index(&BTreeMap::from([(
                "agent/example".into(),
                st3_conversation_ui::Timeline {
                    items,
                    session_id: Some("session/example".into()),
                    ..Default::default()
                },
            )]));
            let key = content
                .request_tool("agent/example", "view-entry")
                .pop()
                .unwrap();
            let full = (0..500)
                .map(|line| format!("synthetic-{kind}-{line}"))
                .collect::<Vec<_>>()
                .join("\n");
            view[field] = Value::String(full);
            content.complete(
                key,
                Ok((
                    "application/json".into(),
                    serde_json::to_vec(&view).unwrap(),
                )),
            );
            let shown = words(&render(&content, &entries, true));
            assert!(
                shown.contains(&format!("synthetic-{kind}-499")),
                "{kind}: {shown}"
            );
            assert!(!shown.contains("Full referenced content (JSON)"), "{kind}");
            assert!(!shown.contains(marker), "{kind}");
        }
    }

    #[test]
    fn fetched_result_view_keeps_original_call_opening_rows() {
        let call: st3_client::TimelineEntry = serde_json::from_value(serde_json::json!({
            "id":"call","sequence":1,"revision":1,"timestamp":"2026-10-08T10:00:00Z","role":"assistant","type":"tool_call","final":true,
            "body":{"call_id":"paired","name":"eval","arguments":{},"blocks":[{"id":"call-block","kind":"tool_call","source_type":"native","payload":{},"view":{"type":"eval","language":"typescript","code":"synthetic opening code"}}]}
        })).unwrap();
        let result: st3_client::TimelineEntry = serde_json::from_value(serde_json::json!({
            "id":"result","sequence":2,"revision":1,"timestamp":"2026-10-08T10:01:00Z","role":"tool","type":"tool_result","final":true,
            "body":{"call_id":"paired","status":"success","media_type":"text/plain","content":"synthetic result","blocks":[{"id":"result-block","kind":"tool_output","source_type":"native","payload":{},"view":{"type":"edit","diff":"[st truncated this native timeline value]"},"continuation":{"ref":"result-view","media_type":"application/json","reason":"size-limit"}}]}
        })).unwrap();
        let items = vec![call, result];
        let entries = super::super::adapt::conversation(&items, &BTreeMap::new());
        let mut content = Content::default();
        content.index(&BTreeMap::from([(
            "agent/example".into(),
            st3_conversation_ui::Timeline {
                items,
                session_id: Some("session/example".into()),
                ..Default::default()
            },
        )]));
        let key = content.request_tool("agent/example", "call").pop().unwrap();
        content.complete(
            key,
            Ok((
                "application/json".into(),
                br#"{"type":"edit","diff":"+ synthetic full result"}"#.to_vec(),
            )),
        );
        let shown = words(&render(&content, &entries, true));
        assert!(shown.contains("synthetic opening code"));
        assert!(shown.contains("+ synthetic full result"));
    }

    /// An invented picture drawn here: rings over a gradient, `width`×`height`.
    #[test]
    fn a_small_file_declaring_a_huge_picture_is_refused_before_decoding() {
        // A real 4×4 PNG whose header then declares 100000×100000 pixels, its checksum fixed.
        let mut bytes = drawn_png(4, 4);
        bytes[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        assert!(bytes.len() < 1024);
        let Err(error) = Fetched::decode(bytes) else { panic!("a huge declared picture decoded") };
        assert!(error.starts_with("Cannot display this image"), "{error}");
        // An ordinary picture still decodes and is scaled.
        let Ok(Fetched::Image { width, image, .. }) = Fetched::decode(drawn_png(2000, 20)) else { panic!() };
        assert_eq!((width, image.width()), (2000, MAX_EDGE));
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn drawn_png(width: u32, height: u32) -> Vec<u8> {
        let picture = image::RgbImage::from_fn(width, height, |x, y| {
            let ring = ((x as i64 - width as i64 / 2).pow(2) + (y as i64 - height as i64 / 2).pow(2))
                .isqrt()
                / 12
                % 2;
            image::Rgb([(x * 200 / width.max(1)) as u8, 120 + 100 * ring as u8, (y * 180 / height.max(1)) as u8])
        });
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(picture)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }

    fn mail_timeline(from: &str, to: &str, size: u64) -> BTreeMap<String, st3_conversation_ui::Timeline> {
        let items = serde_json::from_value(serde_json::json!([
            {"id":"m","sequence":1,"revision":1,"timestamp":"2026-10-13T10:00:00Z","role":"user","type":"message","final":true,
             "body":{"message_id":"message/sketch","from":from,"to":to,"attachments":[
                {"blob":format!("blob/{}", "ab".repeat(32)),"sha256":"ab".repeat(32),"media_type":"image/png",
                 "name":"/home/example/north-bed.png","size":size,"origin":"host/far"}]}},
            {"id":"c","sequence":2,"revision":1,"timestamp":"2026-10-13T10:00:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"two sketches for the north bed"}}
        ]))
        .unwrap();
        BTreeMap::from([(
            "agent/example".into(),
            st3_conversation_ui::Timeline { items, session_id: Some("session/example".into()), ..Default::default() },
        )])
    }

    fn mail_entries(timelines: &BTreeMap<String, st3_conversation_ui::Timeline>) -> Vec<super::super::view::Entry> {
        let names = BTreeMap::from([("person/ada".to_owned(), "you".to_owned())]);
        super::super::adapt::conversation(&timelines["agent/example"].items, &names)
    }

    #[test]
    fn a_message_image_names_its_type_and_size_loads_inline_and_opens() {
        let png = drawn_png(320, 200);
        let timelines = mail_timeline("person/ada", "agent/example", png.len() as u64);
        let entries = mail_entries(&timelines);
        let mut content = Content::default();
        content.index(&timelines);
        let doc = render(&content, &entries, false);
        let text = whole(&doc);
        // One line per image, never a path: its name, type and the size st recorded.
        let expected = format!("▣ north-bed.png · image/png · {} KB  open · load inline (Ctrl+U)", png.len().div_ceil(1024));
        assert!(text.contains(&expected), "{text}");
        assert_eq!(text.matches("north-bed.png").count(), 1, "{text}");
        assert!(!text.contains("/home/example"));
        let key = doc.targets.iter().find_map(|target| match &target.hit {
            Hit::ContentImage(key) => Some(key.clone()),
            _ => None,
        }).expect("the line loads the image");
        assert_eq!(key.conversation, "message/sketch");
        assert_eq!(key.reference, format!("blob/{}", "ab".repeat(32)));
        assert!(doc.targets.iter().any(|target| matches!(&target.hit,
            Hit::Pane(st3_conversation_ui::PaneIntent::Image(image)) if image.message == "message/sketch")));
        // Loading says so; the image then reserves its rows and draws once encoded.
        assert!(content.toggle_image(&key));
        assert!(whole(&render(&content, &entries, false)).contains("open · loading…"));
        content.complete(key.clone(), Ok((String::new(), png)));
        let doc = render(&content, &entries, false);
        assert!(whole(&doc).contains("320×200"), "{}", whole(&doc));
        let inline = doc.targets.iter().find(|target| matches!(target.hit, Hit::InlineImage(_))).unwrap();
        let reserved = &doc.lines[inline.line..inline.line + IMAGE_ROWS];
        assert!(reserved.iter().all(|line| line.spans.iter().all(|span| span.content.trim().is_empty())));
        let area = Rect::new(0, 0, 32, IMAGE_ROWS as u16);
        let mut buffer = Buffer::empty(area);
        content.draw_image(&key, 32, 0, area, &mut buffer);
        content.build_wanted(&ratatui_image::picker::Picker::halfblocks());
        content.draw_image(&key, 32, 0, area, &mut buffer);
        assert!(buffer.content.iter().any(|cell| cell.symbol() != " "));
    }

    #[test]
    fn an_image_the_viewport_cuts_draws_its_visible_rows_in_every_protocol() {
        let timelines = mail_timeline("person/ada", "agent/example", 71);
        let mut content = Content::default();
        content.index(&timelines);
        let key = content.mail.keys().next().unwrap().clone();
        assert!(content.toggle_image(&key));
        content.complete(key.clone(), Ok((String::new(), drawn_png(320, 200))));
        let mut kitty = ratatui_image::picker::Picker::halfblocks();
        kitty.set_protocol_type(ratatui_image::picker::ProtocolType::Kitty);
        for picker in [ratatui_image::picker::Picker::halfblocks(), kitty] {
            // The top five rows scrolled away: the rest fill the four rows still on screen.
            let area = Rect::new(0, 0, 32, 4);
            content.protocols.get_mut().clear();
            content.draw_image(&key, 32, 5, area, &mut Buffer::empty(area));
            content.build_wanted(&picker);
            let mut buffer = Buffer::empty(area);
            content.draw_image(&key, 32, 5, area, &mut buffer);
            for row in 0..4 {
                assert!(
                    (0..32).any(|column| buffer[(column, row)].symbol() != " "),
                    "{:?} row {row} is blank",
                    picker.protocol_type()
                );
            }
        }
    }

    #[test]
    fn mail_between_others_still_shows_its_images() {
        let timelines = mail_timeline("external/discord/user/404", "agent/example", 69);
        let entries = mail_entries(&timelines);
        let mut content = Content::default();
        content.index(&timelines);
        let text = whole(&render(&content, &entries, false));
        assert!(text.contains("▣ north-bed.png · image/png · 1 KB  open"), "{text}");
    }

    #[test]
    fn an_unreachable_owner_is_said_on_the_image_line_until_retried() {
        let timelines = mail_timeline("person/ada", "agent/example", 70);
        let entries = mail_entries(&timelines);
        let mut content = Content::default();
        content.index(&timelines);
        let key = content.tool_images("agent/example", "message/sketch").pop().unwrap();
        assert!(content.toggle_image(&key));
        content.complete(key.clone(), Err("far cannot be reached right now".into()));
        let text = whole(&render(&content, &entries, false));
        assert!(text.contains("far cannot be reached right now · load inline to retry"), "{text}");
        assert!(content.toggle_image(&key), "a failed image is read again on request");
    }

    #[test]
    fn a_huge_image_is_scaled_before_it_is_held_and_released_when_far() {
        let Ok(Fetched::Image { image, width, height, .. }) = Fetched::decode(drawn_png(4000, 1000)) else {
            panic!("decodes");
        };
        assert_eq!((width, height), (4000, 1000));
        assert_eq!((image.width(), image.height()), (MAX_EDGE, MAX_EDGE / 4));
        let timelines = mail_timeline("person/ada", "agent/example", 70);
        let entries = mail_entries(&timelines);
        let mut content = Content::default();
        content.index(&timelines);
        let key = content.tool_images("agent/example", "message/sketch").pop().unwrap();
        assert!(content.toggle_image(&key));
        content.finish(key.clone(), Ok(Fetched::Image {
            media_type: "image/png".into(), size: 70, width, height, image,
        }));
        content.release_far(&BTreeSet::from([key.clone()]));
        assert!(matches!(content.loaded.get(&key), Some(Loaded::Image { .. })), "near images stay");
        content.release_far(&BTreeSet::new());
        assert!(content.loaded.is_empty() && content.protocols.borrow().is_empty());
        // What it is stays on its line, and the person's choice to show it brings it back.
        assert!(whole(&render(&content, &entries, false)).contains("4000×1000"));
        assert!(content.shown_released(&key));
        assert!(content.request_visible_image(&key));
    }

    #[test]
    fn a_tool_results_own_image_is_loadable_under_its_call() {
        let items = serde_json::from_value(serde_json::json!([
            {"id":"call","sequence":1,"revision":1,"timestamp":"2026-10-13T10:00:00Z","role":"assistant","type":"tool_call","final":true,
             "body":{"call_id":"t1","name":"Read","arguments":{"file_path":"pond.png"}}},
            {"id":"result","sequence":2,"revision":1,"timestamp":"2026-10-13T10:00:00Z","role":"tool","type":"tool_result","final":true,
             "body":{"call_id":"t1","status":"success","media_type":"application/json",
                "content":[{"type":"image","content":{"ref":"pond-ref","media_type":"image/png","size":1269,"reason":"on-demand"}}],
                "blocks":[{"id":"result/0","kind":"tool_output","source_type":"tool_result","payload":{"body_ref":true}}]}}
        ]))
        .unwrap();
        let timelines = BTreeMap::from([(
            "agent/example".to_owned(),
            st3_conversation_ui::Timeline { items, session_id: Some("session/example".into()), ..Default::default() },
        )]);
        let mut content = Content::default();
        content.index(&timelines);
        let images = content.tool_images("agent/example", "call");
        assert_eq!(images.len(), 1, "the image in the result's content is found");
        assert_eq!(images[0].reference, "pond-ref");
        let entries = super::super::adapt::conversation(&timelines["agent/example"].items, &BTreeMap::new());
        let text = whole(&render(&content, &entries, true));
        assert!(text.contains("[image image/png, 2 KB] (1269 bytes)  load inline (Ctrl+U)"), "{text}");
    }

    #[test]
    fn an_asks_images_get_lines_that_load_them_on_its_card() {
        let image = st3_conversation_ui::MailImage {
            sha256: "cd".repeat(32),
            message: "step-run/ask-1/ask".into(),
            media_type: "image/png".into(),
            name: Some("north-bed.png".into()),
            size: 71968,
        };
        let mut content = Content::default();
        content.index_requests(vec![image.clone()]);
        let mut doc = Doc::new();
        doc.line(Line::from("North bed plan"));
        doc.targets.push(Target { line: 1, column: 0, width: 40, hit: Hit::Pane(st3_conversation_ui::PaneIntent::Image(image.clone())) });
        doc.line(Line::from("   ▣ north-bed.png  open"));
        doc.line(Line::from("choose one"));
        content.expand_requests(&mut doc, 80);
        assert!(whole(&doc).contains("▣ north-bed.png · image/png · 71 KB  open · load inline (Ctrl+U)"), "{}", whole(&doc));
        let key = image_key(&image);
        assert!(content.toggle_image(&key));
        content.complete(key.clone(), Ok((String::new(), drawn_png(64, 40))));
        let mut doc = Doc::new();
        doc.line(Line::from("North bed plan"));
        doc.targets.push(Target { line: 1, column: 0, width: 40, hit: Hit::Pane(st3_conversation_ui::PaneIntent::Image(image.clone())) });
        doc.line(Line::from("   ▣ north-bed.png  open"));
        doc.line(Line::from("choose one"));
        content.expand_requests(&mut doc, 80);
        assert_eq!(doc.lines.len(), 3 + IMAGE_ROWS);
        assert!(doc.targets.iter().any(|target| target.line == 2 && matches!(target.hit, Hit::InlineImage(_))));
        assert_eq!(whole(&doc).lines().last(), Some("choose one"));
        // An image no open ask carries leaves memory.
        content.index_requests(Vec::new());
        assert!(content.loaded.is_empty());
    }

    #[test]
    fn a_persons_pasted_image_shows_once() {
        // As st serves a Claude turn with text and an image: the message, whose internal source
        // record names the image, then the image as content of its own.
        let items = serde_json::from_value(serde_json::json!([
            {"id":"turn","sequence":1,"revision":1,"timestamp":"2026-10-13T10:00:00Z","role":"user","type":"message","final":true,
             "body":{"message_id":"u1","reply_to":null,"blocks":[{"id":"turn/source","kind":"source_record","source_type":"claude","visibility":"internal",
                "payload":{"raw":{"type":"user","message":{"role":"user","content":[{"type":"text","text":"what is growing here?"},
                    {"type":"image","content":{"ref":"lawn-ref","media_type":"image/png","reason":"on-demand","size":71968}}]}}}}]}},
            {"id":"picture","sequence":2,"revision":1,"timestamp":"2026-10-13T10:00:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"[image · load from owner]","blocks":[{"id":"picture/0","kind":"image","source_type":"image","payload":{},
                "continuation":{"ref":"lawn-ref","media_type":"image/png","size":71968,"reason":"on-demand"}}]}}
        ]))
        .unwrap();
        let timelines = BTreeMap::from([(
            "agent/example".to_owned(),
            st3_conversation_ui::Timeline { items, session_id: Some("session/example".into()), ..Default::default() },
        )]);
        let mut content = Content::default();
        content.index(&timelines);
        let entries = super::super::adapt::conversation(&timelines["agent/example"].items, &BTreeMap::new());
        for density in [st3_conversation_ui::Density::Full, st3_conversation_ui::Density::Simple] {
            for open in [false, true] {
                let text = whole(&render_as(&content, &entries, open, density));
                assert_eq!(text.matches("[image image/png, 71 KB]").count(), 1, "{text}");
            }
        }
    }

    #[test]
    fn an_asks_image_on_a_card_keeps_the_cards_frame() {
        use super::super::doc::DocExt as _;
        let image = st3_conversation_ui::MailImage {
            sha256: "ef".repeat(32),
            message: "step-run/ask-2/ask".into(),
            media_type: "image/png".into(),
            name: Some("pond.png".into()),
            size: 2048,
        };
        let mut content = Content::default();
        content.index_requests(vec![image.clone()]);
        let key = image_key(&image);
        assert!(content.toggle_image(&key));
        content.complete(key.clone(), Ok((String::new(), drawn_png(64, 40))));
        let mut inner = Doc::new();
        inner.line(Line::from("Dig the pond here?"));
        inner.targets.push(Target { line: 1, column: 0, width: 40, hit: Hit::Pane(st3_conversation_ui::PaneIntent::Image(image.clone())) });
        inner.line(Line::from("   ▣ pond.png  open"));
        let mut doc = Doc::new();
        doc.card("request", theme::MAUVE, true, inner, 60);
        content.expand_requests(&mut doc, 60);
        let text = whole(&doc);
        assert!(doc.lines.iter().all(|line| line.width() == 60), "{text}");
        let row = text.lines().find(|line| line.contains("pond.png")).unwrap();
        assert!(row.starts_with("┃    ▣ pond.png · image/png · 1 KB, 64×40  open · hide") && row.ends_with("… ┃"), "{row}");
        let open = doc.targets.iter().find(|target| matches!(target.hit, Hit::Pane(_))).unwrap();
        assert_eq!(&row.chars().skip(open.column as usize).take(4).collect::<String>(), "open");
        let rows = doc.targets.iter().find(|target| matches!(target.hit, Hit::InlineImage(_))).unwrap();
        assert_eq!((rows.line, rows.column, rows.width), (3, 2, 54), "the image sits inside the frame");
        assert!(text.lines().skip(3).take(IMAGE_ROWS).all(|line| line.starts_with("┃ ") && line.ends_with(" ┃")), "{text}");
    }
}
