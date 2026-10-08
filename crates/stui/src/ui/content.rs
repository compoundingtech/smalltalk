//! On-demand conversation content. Neither bytes nor terminal image protocols leave memory.

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
};

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
        image: image::DynamicImage,
    },
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
    // Refs are opaque. A full body must decode as the source's body type and
    // match its tool identity. Otherwise treat a clipped payload as a payload
    // only when metadata/view were not clipped; ambiguous subtrees stay raw.
    let payload = clipped(&block.payload)
        && !block.metadata.as_ref().is_some_and(clipped)
        && !block.view.as_ref().is_some_and(clipped);
    let body = match &source.body {
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
        TimelineBody::Content(original) if matches!(block.kind.as_str(), "text" | "reasoning") => {
            if let Ok(full) =
                serde_json::from_value::<st3_client::TimelineContentBody>(value.clone())
            {
                TimelineBody::Content(Box::new(full))
            } else if payload && value.is_string() {
                let mut full = original.clone();
                full.text = value.as_str().map(str::to_owned);
                TimelineBody::Content(full)
            } else {
                return None;
            }
        }
        _ => return None,
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

#[derive(Default)]
pub struct Content {
    groups: BTreeMap<(String, String), Vec<Group>>,
    sources: BTreeMap<String, BTreeMap<String, st3_client::TimelineEntry>>,
    loaded: BTreeMap<Key, Loaded>,
    shown: BTreeSet<Key>,
    protocols: RefCell<BTreeMap<Key, (u16, u16, ratatui_image::protocol::Protocol)>>,
    pub scroll_to: RefCell<BTreeMap<String, String>>,
    pub focused: BTreeMap<String, String>,
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
            .collect();
        self.loaded.retain(|key, _| active.contains(key));
        self.shown.retain(|key| active.contains(key));
        self.protocols
            .borrow_mut()
            .retain(|key, _| active.contains(key));
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
            self.shown.insert(key.clone());
            return self.request(key);
        }
        if !self.shown.insert(key.clone()) {
            self.shown.remove(key);
            self.loaded.remove(key);
            self.protocols.get_mut().remove(key);
            return false;
        }
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

    pub fn complete(&mut self, key: Key, result: Result<(String, Vec<u8>), String>) {
        // An old in-flight response must never resurrect a changed revision.
        if !self.loaded.contains_key(&key) {
            return;
        }
        let loaded = match result {
            Err(error) => Loaded::Failed(error),
            Ok((media_type, bytes)) => {
                if media_type == "application/json" {
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
                } else {
                    let size = bytes.len() as u64;
                    match image::guess_format(&bytes).and_then(|format| {
                        image::load_from_memory_with_format(&bytes, format)
                            .map(|image| (format, image))
                    }) {
                        Ok((format, image)) => Loaded::Image {
                            media_type: format.to_mime_type().into(),
                            size,
                            image,
                        },
                        Err(error) => Loaded::Failed(format!("Cannot display this image: {error}")),
                    }
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
            let mut extra = Doc::new();
            let mut replace = false;
            for group in groups {
                for (key, reference) in &group.refs {
                    let loaded = self.loaded.get(key);
                    let image = reference.reason.as_deref() == Some("on-demand")
                        || reference.media_type.starts_with("image/")
                        || matches!(loaded, Some(Loaded::Image { .. }));
                    if image {
                        let (media, size) = match loaded {
                            Some(Loaded::Image {
                                media_type, size, ..
                            }) => (media_type.as_str(), Some(*size)),
                            _ => (reference.media_type.as_str(), reference.size),
                        };
                        let exact =
                            size.map_or_else(String::new, |size| format!(" ({size} bytes)"));
                        let size = size.map_or_else(
                            || "size unknown".into(),
                            |size| format!("{} KB", size.div_ceil(1024)),
                        );
                        let action = match loaded {
                            Some(Loaded::Loading) => "loading…".to_owned(),
                            Some(Loaded::Failed(error)) => {
                                format!("{error} · load inline to retry")
                            }
                            _ if self.shown.contains(key) => "hide inline".into(),
                            _ => "load inline (Ctrl+U)".into(),
                        };
                        extra.targets.push(Target {
                            line: extra.lines.len(),
                            column: 0,
                            width: width as u16,
                            hit: Hit::ContentImage(key.clone()),
                        });
                        extra.line(Line::from(Span::styled(
                            format!("  [image {media}, {size}]{exact}  {action}"),
                            theme::fg(theme::LAVENDER),
                        )));
                        if open
                            && self.shown.contains(key)
                            && matches!(loaded, Some(Loaded::Image { .. }))
                        {
                            extra.targets.push(Target {
                                line: extra.lines.len(),
                                column: 0,
                                width: width as u16,
                                hit: Hit::InlineImage(key.clone()),
                            });
                            for _ in 0..12 {
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
            let start = if replace { entries[index].1 } else { at };
            let shift = extra.lines.len() as isize - (at - start) as isize;
            if replace {
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

    pub fn draw_image(
        &self,
        picker: &ratatui_image::picker::Picker,
        key: &Key,
        area: Rect,
        buf: &mut Buffer,
    ) {
        if !self.shown.contains(key) {
            return;
        }
        let Some(Loaded::Image { image, .. }) = self.loaded.get(key) else {
            return;
        };
        let mut protocols = self.protocols.borrow_mut();
        if !protocols
            .get(key)
            .is_some_and(|(width, height, _)| *width == area.width && *height == area.height)
        {
            // One encoding per image, not one for every size visited while resizing.
            protocols.remove(key);
            let Ok(protocol) = picker.new_protocol(
                image.clone(),
                ratatui::layout::Size::new(area.width, area.height),
                ratatui_image::Resize::Fit(None),
            ) else {
                return;
            };
            protocols.insert(key.clone(), (area.width, area.height, protocol));
        }
        if let Some((_, _, protocol)) = protocols.get(key) {
            use ratatui::widgets::Widget as _;
            ratatui_image::Image::new(protocol).render(area, buf);
        }
    }
}

pub async fn fetch(client: &Client, key: &Key) -> Result<(String, Vec<u8>), String> {
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
            None => return Ok((chunk.media_type, bytes)),
        }
    }
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
            st3_conversation_ui::Density::Full,
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
        assert!(words(&after).contains(&format!("[image image/png, {} KB]", size.div_ceil(1024))));
        assert!(
            after
                .targets
                .iter()
                .any(|target| matches!(target.hit, Hit::InlineImage(_)))
        );
        let area = Rect::new(0, 0, 16, 12);
        let mut buffer = Buffer::empty(area);
        content.draw_image(
            &ratatui_image::picker::Picker::halfblocks(),
            &key,
            area,
            &mut buffer,
        );
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
                content.draw_image(&picker, &key, area, &mut Buffer::empty(area));
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
}
