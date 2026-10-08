use crate::{Body, Entry, MailImage, ToolState};
use serde_json::Value;
use st3_client::{TimelineBody, TimelineEntry, TimelineRole, TimelineToolStatus};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Why a conversation cannot be shown whole, when st could not read the harness's transcript.
/// st then sends only the Small Talk around it, which reads as a conversation with the agent's
/// side missing; Nathan's rule (2026-10-01) is to show neither half then, and say why, with the
/// transcript's path so it can be reported.
pub fn unreadable_transcript(timeline: &[TimelineEntry]) -> Option<String> {
    let error = timeline.iter().rev().find_map(|entry| match &entry.body {
        TimelineBody::Error(error) if error.code == "transcript-not-bound" => Some(error),
        _ => None,
    })?;
    // Pending binding is shown as an availability notice alongside authorized live entries.
    if not_yet(error) {
        return None;
    }
    let reason = error
        .message
        .strip_prefix("transcript not bound: ")
        .unwrap_or(&error.message);
    let reason = bounded_preview(reason, 256);
    Some(
        match error.details.get("transcript").and_then(Value::as_str) {
            Some(path) => {
                format!("This conversation could not be loaded: {reason} (transcript {path})")
            }
            None => format!("This conversation could not be loaded: {reason}"),
        },
    )
}

/// st's notice that the transcript is not bound yet; this does not prove an idle harness.
fn not_yet(error: &st3_client::TimelineErrorBody) -> bool {
    error.details.get("not_yet").and_then(Value::as_bool) == Some(true)
}

/// One conversation: the harness transcript and Small Talk messages, in time order.
/// Draw one conversation as st joined it: the harness's turns and the agent's Small Talk, in
/// the order st sent them.
/// How a message's signature reads beside its sender: the device that signed it and whether
/// that checks ("✓ example phone (secure enclave)"). A message st holds no signature for is usually
/// just old, so it says nothing and a conversation is not covered in "unsigned".
pub fn signature_mark(provenance: &st3_client::MessageProvenance) -> Option<String> {
    let who = provenance
        .device
        .clone()
        .or_else(|| provenance.signer.clone())
        .unwrap_or_default();
    let reason = |text: &str| {
        provenance
            .reason
            .as_deref()
            .map(|reason| format!("{text}: {reason}"))
            .unwrap_or_else(|| text.to_owned())
    };
    match provenance.verdict.as_str() {
        "verified" => Some(format!("✓ {who}").trim_end().to_owned()),
        "held" => Some(format!("⚠ {}", reason("signature held"))),
        "invalid" => Some(format!("✕ {}", reason("signature invalid"))),
        _ => None,
    }
}

/// Said once at the start of a conversation st read only the newest part of.
const NATIVE_PREFIX_NOTE: &str =
    "Earlier history is not shown: st reads only the newest part of this agent's transcript";

pub fn conversation(timeline: &[TimelineEntry], names: &BTreeMap<String, String>) -> Vec<Entry> {
    conversation_with_filters(timeline, names, crate::DEFAULT_FILTERS)
}

pub fn conversation_with_filters(
    timeline: &[TimelineEntry],
    names: &BTreeMap<String, String>,
    filters: &[crate::DisplayFilter],
) -> Vec<Entry> {
    if filters.is_empty() {
        // JSON escapes terminal controls reversibly; the data itself remains unchanged.
        return timeline
            .iter()
            .map(|entry| Entry {
                id: entry.id.clone(),
                at: clock(&entry.timestamp),
                body: Body::User(serde_json::to_string_pretty(entry).expect("timeline JSON")),
            })
            .collect();
    }
    let displayed = timeline
        .iter()
        .map(|entry| filtered_entry(entry, filters))
        .collect::<Vec<_>>();
    let timeline = &displayed;
    let name = |id: &str| -> String {
        names.get(id).cloned().unwrap_or_else(|| match id {
            "daemon/runtime" => "st".into(),
            id if id.starts_with("person/") => id.trim_start_matches("person/").to_owned(),
            id => short(id),
        })
    };
    let mut stamped: Vec<(String, Entry)> = Vec::new();
    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    // Claude emits a loaded skill as user content after the Skill result, without a call id.
    // Match its base directory to a still-pending Skill call in this turn.
    let mut skills: Vec<(String, usize)> = Vec::new();
    let mut expanded_skills = BTreeSet::new();
    let mut delivery: BTreeMap<String, bool> = BTreeMap::new();
    // Graph messages the agent's harness received, from its own transcript.
    let mut delivered: BTreeSet<String> = BTreeSet::new();
    // A Small Talk message is two entries: who wrote to whom, then what they wrote. A
    // harness transcript heads its own turns with message entries too; only a graph message,
    // `message/…`, is Small Talk.
    let mut mail: Option<(&TimelineEntry, &st3_client::TimelineMessageBody)> = None;
    // A harness that wraps a delivery in its own prompt repeats mail the stream may already show.
    let shown = timeline
        .iter()
        .filter_map(|entry| match &entry.body {
            TimelineBody::Message(message) if message.message_id.starts_with("message/") => {
                Some(message.message_id.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    for entry in timeline {
        if mail.is_none() && is_bookkeeping(entry, filters) {
            continue;
        }
        let at = clock(&entry.timestamp);
        if let TimelineBody::Message(message) = &entry.body {
            for block in message
                .blocks
                .iter()
                .filter(|block| block.kind == "raw_text")
            {
                if let Some(text) = block.payload.get("text").and_then(Value::as_str) {
                    stamped.push((
                        entry.timestamp.clone(),
                        Entry {
                            id: block.id.clone(),
                            at: at.clone(),
                            body: Body::Event(format!(
                                "[unreadable message]\n{}",
                                crate::clean_message_text_with_filters(text, filters)
                            )),
                        },
                    ));
                }
            }
            if let Some((pending, message)) = mail.take() {
                append_unpaired_media(&mut stamped, pending, message, &name);
            }
            if message.message_id.starts_with("message/") {
                mail = Some((entry, message));
            } else {
                append_unpaired_media(&mut stamped, entry, message, &name);
            }
            continue;
        }
        if let TimelineBody::Content(content) = &entry.body
            && let Some((_, message)) = mail.take()
        {
            let mut body = content_text(content);
            for attachment in &message.attachments {
                if !attachment.media_type.starts_with("image/")
                    && content.attachment_id.as_deref() != Some(attachment.blob.as_str())
                    && content.attachment_id.as_deref() != Some(attachment.sha256.as_str())
                {
                    if !body.is_empty() {
                        body.push('\n');
                    }
                    body.push_str(&format!(
                        "[media: {} · {}]",
                        attachment.media_type, attachment.sha256
                    ));
                }
            }
            let from = message.from.as_deref().unwrap_or_default();
            let body = if from == "daemon/runtime" {
                // Step-ready pings are graph events, not conversation.
                Body::Event(
                    message
                        .title
                        .clone()
                        .unwrap_or_else(|| body.lines().next().unwrap_or("").to_owned()),
                )
            } else {
                // An older st sends neither side; say what it is rather than draw a blank.
                let (from, to) = match (from, message.to.as_deref()) {
                    ("", None) => ("Small Talk".to_owned(), String::new()),
                    (from, to) => (name(from), name(to.unwrap_or_default())),
                };
                let images = message
                    .attachments
                    .iter()
                    .filter(|attachment| attachment.media_type.starts_with("image/"))
                    .map(|attachment| MailImage {
                        sha256: attachment.sha256.clone(),
                        message: message.message_id.clone(),
                        media_type: attachment.media_type.clone(),
                        name: attachment.name.clone(),
                        size: attachment.size,
                    })
                    .collect::<Vec<_>>();
                Body::Mail {
                    from,
                    to,
                    subject: message.title.clone().unwrap_or_default(),
                    // A message may be only its images.
                    body: if body.is_empty() && images.is_empty() {
                        "(notification)".into()
                    } else {
                        body
                    },
                    delivered: false,
                    dictated: message.tags.iter().any(|tag| tag == "dictated"),
                    signed: message.provenance.as_ref().and_then(signature_mark),
                    images,
                }
            };
            stamped.push((
                entry.timestamp.clone(),
                Entry {
                    id: message.message_id.clone(),
                    at,
                    body,
                },
            ));
            continue;
        }
        let body = match (&entry.role, &entry.body) {
            (TimelineRole::User | TimelineRole::System, TimelineBody::Content(content)) => {
                let raw = content.text.as_deref().unwrap_or("");
                if entry.role == TimelineRole::User
                    && let Some(skill) = loaded_skill(raw)
                    && let Some(pending) = skills.iter().rposition(|(name, _)| name == skill)
                {
                    let (_, index) = skills.remove(pending);
                    if let Body::Tool { state, output, .. } = &mut stamped[index].1.body {
                        *state = ToolState::Ok;
                        *output = raw.trim().lines().map(str::to_owned).collect();
                        expanded_skills.insert(index);
                        continue;
                    }
                }
                // Structural harness envelopes become what they mean.
                if content.blocks.iter().any(|block| block.kind != "text") || raw.starts_with("[unrecognized ") {
                    stamped.push((entry.timestamp.clone(), Entry { id:entry.id.clone(), at, body: if entry.role == TimelineRole::User { Body::User(crate::clean_message_text_with_filters(raw, filters)) } else { Body::Event(crate::clean_message_text_with_filters(raw, filters)) } }));
                    continue;
                }
                let mut bodies = harness_bodies(
                    entry.role == TimelineRole::User,
                    content.text.as_deref().unwrap_or(""),
                    &shown,
                    &mut delivered,
                    filters,
                );
                if bodies.iter().any(|body| matches!(body, Body::User(_))) {
                    skills.clear();
                }
                if content.attachment_id.is_some()
                    || (content.text.is_none() && content.media_type != "text/plain")
                {
                    bodies.push(Body::Event(format!(
                        "[media: {} · {}]",
                        content.media_type,
                        content.attachment_id.as_deref().unwrap_or("reference unavailable")
                    )));
                }
                for (index, mut body) in bodies.into_iter().enumerate() {
                    if let Body::Mail { from, to, .. } = &mut body {
                        *from = name(from);
                        *to = name(to);
                    }
                    stamped.push((
                        entry.timestamp.clone(),
                        Entry {
                            id: format!("{}#{index}", entry.id),
                            at: at.clone(),
                            body,
                        },
                    ));
                }
                continue;
            }
            (TimelineRole::Assistant, TimelineBody::Content(content)) => {
                skills.clear();
                let text = content_text(content);
                if text.is_empty() {
                    continue;
                }
                Body::Assistant(text)
            }
            (TimelineRole::Tool, TimelineBody::Content(content)) => {
                let text = content_text(content);
                if text.is_empty() {
                    continue;
                }
                let mut lines = text.lines().map(str::to_owned);
                Body::Tool {
                    title: lines.next().unwrap_or_default(),
                    state: ToolState::Ok,
                    output: lines.collect(),
                }
            }
            (_, TimelineBody::ToolCall(call)) => {
                tools.insert(call.call_id.clone(), stamped.len());
                if call.name == "Skill"
                    && let Some(skill) = call.arguments.get("skill").and_then(Value::as_str)
                {
                    skills.push((
                        skill.rsplit(':').next().unwrap_or(skill).to_owned(),
                        stamped.len(),
                    ));
                }
                Body::Tool {
                    title: tool_title(&call.name, &call.arguments),
                    state: ToolState::Running,
                    output: vec![],
                }
            }
            (_, TimelineBody::ToolResult(result)) => {
                let (output, command_failed) = tool_output(&result.content);
                let state = match result.status {
                    TimelineToolStatus::Error => ToolState::Failed,
                    _ if command_failed => ToolState::Failed,
                    _ => ToolState::Ok,
                };
                if let Some(index) = tools.get(&result.call_id).copied()
                    && let Some((
                        _,
                        Entry {
                            body:
                                Body::Tool {
                                    state: slot,
                                    output: out,
                                    ..
                                },
                            ..
                        },
                    )) = stamped.get_mut(index)
                {
                    *slot = state;
                    // Transcript revisions can put the result after the expansion.
                    if !expanded_skills.contains(&index) {
                        *out = output;
                    }
                    continue;
                }
                Body::Tool {
                    title: "tool result".into(),
                    state,
                    output,
                }
            }
            (_, TimelineBody::Error(error)) => match error.code.as_str() {
                "native-delivery-degraded" => {
                    delivery.insert(entry.id.clone(), false);
                    Body::Event(if error.message.contains("unreachable") {
                        "delivery paused · st unreachable".into()
                    } else {
                        "delivery failing · retrying".into()
                    })
                }
                "transcript-not-bound" => {
                    Body::Event(format!(
                        "transcript unavailable: {}",
                        bounded_preview(&error.message, 256)
                    ))
                }
                "native-delivery-recovered" => {
                    delivery.insert(entry.id.clone(), true);
                    Body::Event("delivery resumed".into())
                }
                // A warning is st noting something it is handling, not a failure.
                _ if error.details.get("severity").and_then(Value::as_str) == Some("warning") => {
                    Body::Event(bounded_preview(&error.message, 256).into_owned())
                }
                _ => Body::Event(format!("error: {}", bounded_preview(&error.message, 256))),
            },
            (_, TimelineBody::Status(status)) => Body::Event(format!(
                "status: {}{}",
                match status.status {
                    st3_client::TimelineStatus::Queued => "queued",
                    st3_client::TimelineStatus::Running => "running",
                    st3_client::TimelineStatus::Waiting => "waiting",
                    st3_client::TimelineStatus::Completed => "completed",
                    st3_client::TimelineStatus::Failed => "failed",
                    st3_client::TimelineStatus::Cancelled => "cancelled",
                    st3_client::TimelineStatus::Unknown => "unknown",
                },
                status
                    .detail
                    .as_ref()
                    .map(|detail| format!(" · {detail}"))
                    .unwrap_or_default()
            )),
            (_, TimelineBody::Usage(usage)) => Body::Event(usage_line(usage)),
            (_, TimelineBody::Redaction(redaction)) => Body::Event(format!(
                "content withheld: {} ({} bytes{})",
                redaction.reason,
                redaction.withheld_bytes,
                redaction
                    .withheld_items
                    .map(|items| format!(", {items} items"))
                    .unwrap_or_default()
            )),
            // st reads only the newest part of a long native transcript. That is how it works, not
            // a fault, so it is said in plain words with no sequence numbers (Nathan, 2026-10-06).
            (_, TimelineBody::Truncation(truncation))
                if truncation.reason.contains("native transcript prefix") =>
            {
                let mut notice = NATIVE_PREFIX_NOTE.to_owned();
                if truncation.reason.contains("not fetchable") {
                    notice.push_str("; earlier history is not fetchable through this read");
                }
                Body::Event(notice)
            }
            (_, TimelineBody::Truncation(truncation)) => Body::Event(format!(
                "history omitted: {} (sequences {}–{}{})",
                truncation.reason,
                truncation.omitted_from_sequence,
                truncation.omitted_to_sequence,
                if truncation.continuation_cursor.is_some() {
                    "; older history available"
                } else {
                    ""
                }
            )),
            (_, TimelineBody::Unknown { entry_type, body }) => Body::Event(format!(
                "[unknown {}]\n{body}", bounded_preview(entry_type,64)
            )),
            (_, TimelineBody::Content(content)) => {
                // Attribution may be unknown; authorized conversation content is still visible.
                let text=content_text(content);
                if text.trim().is_empty() {continue;}
                Body::Event(format!("[unknown role]\n{text}"))
            }
            (_, TimelineBody::Message(_)) => unreachable!("messages are handled above"),
        };
        stamped.push((
            entry.timestamp.clone(),
            Entry {
                id: entry.id.clone(),
                at,
                body,
            },
        ));
    }
    if let Some((pending, message)) = mail {
        append_unpaired_media(&mut stamped, pending, message, &name);
    }
    stamped.sort_by(|a, b| a.0.cmp(&b.0));
    // That earlier history is not shown is said at the start, where the history would be, not at
    // the time st stamped the note with (the session's last update: the bottom, by the box).
    stamped.sort_by_key(|(_, entry)| {
        !matches!(&entry.body, Body::Event(text) if text.starts_with(NATIVE_PREFIX_NOTE))
    });
    for (_, entry) in &mut stamped {
        if let Body::Mail {
            delivered: mark, ..
        } = &mut entry.body
        {
            *mark = delivered.contains(&entry.id);
        }
    }
    fold_events(stamped.into_iter().map(|(_, entry)| entry), &delivery)
}

/// Flush refs only after the next message or end proves that no content paired with this envelope.
fn append_unpaired_media(
    stamped: &mut Vec<(String, Entry)>,
    entry: &TimelineEntry,
    message: &st3_client::TimelineMessageBody,
    name: &impl Fn(&str) -> String,
) {
    if message.attachments.is_empty() {
        return;
    }
    let mut text = format!(
        "media from {}: ",
        name(message.from.as_deref().unwrap_or("Small Talk"))
    );
    for (index, attachment) in message.attachments.iter().enumerate() {
        if index != 0 {
            text.push_str(", ");
        }
        write!(text, "{} · {}", attachment.media_type, attachment.sha256).unwrap();
    }
    stamped.push((
        entry.timestamp.clone(),
        Entry {
            id: message.message_id.clone(),
            at: clock(&entry.timestamp),
            body: Body::Event(text),
        },
    ));
}

fn usage_line(usage: &st3_client::TimelineUsageBody) -> String {
    let semantics = match usage.semantics {
        st3_client::TimelineUsageSemantics::Response => "response",
        st3_client::TimelineUsageSemantics::SessionCumulative => "session cumulative",
        st3_client::TimelineUsageSemantics::ContextOccupancy => "context occupancy",
        st3_client::TimelineUsageSemantics::Unknown => "unknown",
    };
    let mut line = format!("usage: {semantics}");
    for (label, value) in [
        ("input", usage.input_tokens),
        ("output", usage.output_tokens),
        ("cached", usage.cached_tokens),
        ("cache write", usage.cache_write_tokens),
        ("total", usage.total_tokens),
        ("context used", usage.context_used_tokens),
        ("context window", usage.context_window_tokens),
    ] {
        if let Some(value) = value {
            write!(line, " · {label} {value}").unwrap();
        }
    }
    if let Some(cost) = usage.cost {
        if let Some(currency) = &usage.currency {
            write!(line, " · cost {currency} {cost}").unwrap();
        } else {
            write!(line, " · cost {cost} (currency unknown)").unwrap();
        }
    }
    line
}

/// omp's `custom` entries that only repeat what the conversation already shows: a tool call's
/// start (its call is an entry of its own) and the end of the session. Other custom entries stay.
const OMP_BOOKKEEPING_CUSTOM: &[&str] = &["tool_execution_start", "session_exit"];

fn omp_bookkeeping(block: &st3_client::TimelineBlock) -> bool {
    block.source_type == "custom"
        && block.payload["raw"]["customType"]
            .as_str()
            .is_some_and(|kind| OMP_BOOKKEEPING_CUSTOM.contains(&kind))
}

/// Harness records that carry no conversation: the transcript's own titles and modes, and a
/// reasoning step whose text the model did not share. Unknown records stay visible.
const BOOKKEEPING_RECORDS: &[&str] = &[
    // Claude's own entries, system records and attachments.
    "last-prompt",
    "ai-title",
    "mode",
    "permission-mode",
    "atis-latch",
    "pr-link",
    "frame-link",
    "bridge-session",
    "queue-operation",
    "cost-state",
    "stop_hook_summary",
    "turn_duration",
    "total_tokens_reminder",
    "deferred_tools_record",
    "silent_turn_reminder",
    "environment",
    "date",
    "skill_listing",
    "command_permissions",
    "edited_text_file",
    // Codex's turn bookkeeping: the messages and calls they mention are entries of their own.
    "event_msg",
    "token_usage_record",
    "turn_context",
    "world_state",
];

/// A record Claude writes about its own session, in kinds that keep appearing (instructions,
/// session context, prompt snapshots, deferred tools, credential organisation, hook summaries):
/// an attachment, a system record or a transcript entry that no renderer knows by name. Claude's
/// own screen shows none of them as conversation, so they are bookkeeping as a class, not one
/// name at a time.
fn claude_context_record(text: &str) -> bool {
    ["attachment", "system", "entry"]
        .iter()
        .any(|kind| text.starts_with(&format!("[unrecognized claude {kind} `")))
}

fn is_bookkeeping(entry: &TimelineEntry, filters: &[crate::DisplayFilter]) -> bool {
    let TimelineBody::Content(content) = &entry.body else {
        return false;
    };
    if !filters.contains(&crate::DisplayFilter::Bookkeeping) || content.blocks.is_empty() {
        return false;
    }
    let shown = content.blocks.iter().filter(|block| {
        !matches!(
            block.visibility.as_deref(),
            Some("internal" | "hidden-by-harness")
        )
    });
    match entry.role {
        TimelineRole::Assistant => {
            shown.clone().next().is_some()
                && shown.into_iter().all(|block| {
                    block.kind == "reasoning"
                        && block.payload["text"]
                            .as_str()
                            .is_none_or(|text| text.trim().is_empty())
                })
        }
        TimelineRole::System => {
            shown.clone().next().is_some()
                && shown.into_iter().all(|block| {
                    block.kind == "unknown"
                        && (BOOKKEEPING_RECORDS.contains(&block.source_type.as_str())
                            || omp_bookkeeping(block)
                            || claude_context_record(content.text.as_deref().unwrap_or("")))
                })
        }
        _ => false,
    }
}

fn filtered_entry(entry: &TimelineEntry, filters: &[crate::DisplayFilter]) -> TimelineEntry {
    use crate::DisplayFilter;
    let mut value = serde_json::to_value(entry).expect("timeline JSON");
    let body = &mut value["body"];
    if filters.contains(&DisplayFilter::InternalBlocks)
        && let Some(blocks) = body.get_mut("blocks").and_then(Value::as_array_mut)
    {
        blocks.retain(|block| {
            !matches!(
                block["visibility"].as_str(),
                Some("internal" | "hidden-by-harness")
            )
        });
    }
    if let Some(raw) = body["text"].as_str() {
        let mut text = raw.to_owned();
        if filters.contains(&DisplayFilter::ContextBlocks)
            && !matches!(entry.role, TimelineRole::User | TimelineRole::System)
        {
            filter_context_blocks(&mut text)
        }
        if !matches!(entry.role, TimelineRole::User | TimelineRole::System) {
            text = crate::clean_message_text_with_filters(&text, filters);
        }
        // The harness's label for a reasoning step is not part of what it said: thinking it did
        // share reads as "Thinking · ..." (empty steps are hidden as bookkeeping).
        if filters.contains(&DisplayFilter::Bookkeeping)
            && entry.role == TimelineRole::Assistant
            && let Some(thought) = raw.strip_prefix("[reasoning]")
        {
            text = format!("Thinking · {}", thought.trim());
        }
        if filters.contains(&DisplayFilter::Excerpts) && raw.starts_with("[unrecognized ") {
            text = bounded_preview(&text, 512).into_owned();
        }
        body["text"] = Value::String(text);
    }
    // The default still shows an unreadable record, with its readable approximation.
    if matches!(&entry.body, TimelineBody::Error(_))
        && let Some(blocks) = body["blocks"].as_array()
    {
        let raw = blocks
            .iter()
            .filter(|block| block["kind"] == "raw_text")
            .filter_map(|block| block["payload"]["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !raw.is_empty() {
            body["message"] = Value::String(format!(
                "{}\n{}",
                body["message"].as_str().unwrap_or(""),
                crate::clean_message_text_with_filters(&raw, filters)
            ));
        }
    }
    serde_json::from_value(value).expect("display copy retains timeline shape")
}

// Context wrappers may mention their own closing tag in inline code.
fn filter_context_blocks(text: &mut String) {
    for tag in CONTEXT_BLOCKS {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        let mut from = 0;
        while let Some(offset) = text[from..].find(&open) {
            let start = from + offset;
            if crate::clean::in_inline_code(text, start)
                || !text[start + open.len()..].starts_with(['>', ' ', '/', '\n'])
            {
                from = start + open.len();
                continue;
            }
            let Some(head_end) = text[start..].find('>').map(|n| start + n + 1) else {
                text.truncate(start);
                break;
            };
            let end = text[head_end..]
                .match_indices(&close)
                .map(|(n, _)| head_end + n)
                .find(|n| !crate::clean::in_inline_code(text, *n));
            text.replace_range(start..end.map_or(text.len(), |n| n + close.len()), "");
            from = start;
        }
    }
}

const CONTEXT_BLOCKS: &[&str] = &[
    "system-reminder",
    "local-command-caveat",
    "environment_context",
    "permissions",
    "collaboration_mode",
    "multi_agent_mode",
    "apps_instructions",
    "plugins_instructions",
    "skills_instructions",
    "user_instructions",
    "developer_instructions",
    "command-message",
    "command-args",
];

fn bounded_preview(text: &str, max: usize) -> Cow<'_, str> {
    let Some((end, _)) = text.char_indices().nth(max) else {
        return Cow::Borrowed(text);
    };
    let mut preview = String::with_capacity(end + '…'.len_utf8());
    preview.push_str(&text[..end]);
    preview.push('…');
    Cow::Owned(preview)
}

/// Preserve authorized attachment references without fetching payloads or decoding unknown bodies.
fn content_text(content: &st3_client::TimelineContentBody) -> String {
    let mut text = content.text.as_deref().unwrap_or("").to_owned();
    if let Some(reference) = &content.attachment_id {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[media: {} · {reference}]", content.media_type));
    } else if text.is_empty() && content.media_type != "text/plain" {
        text = format!("[media: {} · reference unavailable]", content.media_type);
    }
    text
}

/// A delivery pause that recovered is one quiet line, and a run of the same event line is one
/// line with a count: a seat that outlived a dozen st restarts shows one line, not a dozen pairs.
/// `delivery` says which entries paused delivery (`false`) and which resumed it (`true`).
fn fold_events(
    entries: impl Iterator<Item = Entry>,
    delivery: &BTreeMap<String, bool>,
) -> Vec<Entry> {
    let mut folded: Vec<(Entry, usize)> = Vec::new();
    for mut entry in entries {
        let closes_pause = delivery.get(&entry.id) == Some(&true)
            && folded
                .last()
                .is_some_and(|(last, count)| *count == 1 && delivery.get(&last.id) == Some(&false));
        if closes_pause {
            folded.pop();
            entry.body = Body::Event("delivery paused, then resumed".into());
        }
        if let Body::Event(text) = &entry.body
            && let Some((last, count)) = folded.last_mut()
            && matches!(&last.body, Body::Event(previous) if previous == text)
        {
            *count += 1;
            last.at = entry.at;
            continue;
        }
        folded.push((entry, 1));
    }
    folded
        .into_iter()
        .map(|(mut entry, count)| {
            if count > 1
                && let Body::Event(text) = &mut entry.body
            {
                text.push_str(&format!(" ×{count}"));
            }
            entry
        })
        .collect()
}

/// How Claude Code opens the summary it continues from after compacting a conversation.
const COMPACTED: &str = "This session is being continued from a previous conversation";

/// A line that is only a self-closing harness tag, such as `<artifact-content-authored-by-others/>`.
fn self_closing_tag(line: &str) -> bool {
    let line = line.trim();
    line.strip_prefix('<')
        .and_then(|rest| rest.strip_suffix("/>"))
        .is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// Blocks harnesses add to a transcript for the model's benefit. None of it is conversation.
/// Take every `<tag …>…</tag>` block out of `text`, returning the inner texts. A block that
/// never closes runs to the end. Only structural delivery/command wrappers are parsed.
fn take_blocks(text: &mut String, tag: &str) -> Vec<String> {
    let mut found = Vec::new();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut from = 0;
    while let Some(offset) = text[from..].find(&open) {
        let start = from + offset;
        let after = &text[start + open.len()..];
        // `<permissions` must not match `<permissionsfoo`.
        if !after.starts_with(['>', ' ', '/', '\n']) {
            from = start + open.len();
            continue;
        }
        let Some(head_end) = after.find('>').map(|index| start + open.len() + index + 1) else {
            text.truncate(start);
            break;
        };
        let (inner, end) = match text[head_end..].find(&close) {
            Some(index) => (
                text[head_end..head_end + index].to_owned(),
                head_end + index + close.len(),
            ),
            None => (text[head_end..].to_owned(), text.len()),
        };
        found.push(inner);
        text.replace_range(start..end, "");
        from = start;
    }
    found
}

fn field(block: &str, tag: &str) -> Option<String> {
    let mut copy = block.to_owned();
    take_blocks(&mut copy, tag)
        .into_iter()
        .next()
        .map(|value| value.trim().to_owned())
}

fn shorten(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

/// The value of `name="…"` in a tag's head, unescaped.
fn attribute(head: &str, name: &str) -> Option<String> {
    let start = head.find(&format!(" {name}=\""))? + name.len() + 3;
    let end = head[start..].find('"')? + start;
    Some(unescape(&head[start..end]))
}

/// Undo the XML escaping st applies to a delivery's attributes and body.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Take only delivery wrappers that begin a line outside Markdown code fences. A mention
/// in prose or inline code is content, even if it contains a complete example envelope.
fn take_deliveries(text: &mut String, tag: &str) -> Vec<(String, String)> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    let mut fence: Option<(char, usize)> = None;
    let mut consumed = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        if start < consumed {
            continue;
        }
        let trimmed = line.trim();
        if let Some(marker @ ('`' | '~')) = trimmed.chars().next() {
            let length = trimmed.chars().take_while(|c| *c == marker).count();
            if length >= 3 {
                match fence {
                    Some((open, count))
                        if open == marker
                            && length >= count
                            && trimmed[length..].trim().is_empty() =>
                    {
                        fence = None
                    }
                    None => fence = Some((marker, length)),
                    _ => {}
                }
                continue;
            }
        }
        if fence.is_some() || !line.starts_with(&format!("<{tag} ")) {
            continue;
        }
        let Some(head_end) = line.find('>') else {
            continue;
        };
        let head = line[..=head_end].trim();
        let valid = match tag {
            "smalltalk-message" => {
                let id = attribute(head, "id").filter(|id| !id.is_empty());
                line[head_end + 1..].trim().is_empty()
                    && ["from", "to", "sha256"]
                        .iter()
                        .all(|key| attribute(head, key).is_some_and(|value| !value.is_empty()))
                    && attribute(head, "subject").is_some()
                    && id
                        .is_some_and(|id| attribute(head, "graph") == Some(format!("message/{id}")))
            }
            "channel" => {
                attribute(head, "source").is_some_and(|source| channel_source(&source))
                    && attribute(head, "from").is_some()
            }
            _ => false,
        };
        if !valid {
            continue;
        }
        let inner_start = start + head_end + 1;
        let close = format!("</{tag}>");
        let end = text[inner_start..].find(&close).map(|i| inner_start + i);
        let inner_end = end.unwrap_or(text.len());
        consumed = end.map(|end| end + close.len()).unwrap_or(text.len());
        ranges.push((
            start,
            consumed,
            text[inner_start..inner_end].to_owned(),
            head.to_owned(),
        ));
    }
    let found = ranges
        .iter()
        .map(|(_, _, body, head)| (body.clone(), head.clone()))
        .collect();
    for (start, end, _, _) in ranges.into_iter().rev() {
        text.replace_range(start..end, "");
    }
    found
}

/// Whether a `<channel source=…>` is st's own plugin, under any of the names it has had:
/// `plugin:st-channel:st` now, `st3-channel:st3` and `st2-channel:st2` before. Only the name
/// changes; an unrecognised one would leave an empty `<channel>` shell on screen after its
/// message was taken out (Nathan, 2026-10-05).
pub fn channel_source(source: &str) -> bool {
    source
        .strip_prefix("plugin:")
        .and_then(|name| name.split_once("-channel:"))
        .is_some_and(|(left, right)| {
            matches!(left, "st" | "st2" | "st3") && matches!(right, "st" | "st2" | "st3")
        })
}

/// Claude's skill expansion starts with its base directory, whose basename is the skill name.
fn loaded_skill(raw: &str) -> Option<&str> {
    raw.lines()
        .next()?
        .strip_prefix("Base directory for this skill: ")?
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
}

/// A user or system entry from a harness transcript, turned into what a person should see.
/// `shown` holds the graph messages the stream already draws as mail: a delivery of one of
/// those is a line, and a delivery of any other is the mail itself.
pub fn from_harness(is_user: bool, raw: &str, shown: &BTreeSet<String>) -> Vec<Body> {
    harness_bodies(
        is_user,
        raw,
        shown,
        &mut BTreeSet::new(),
        crate::DEFAULT_FILTERS,
    )
}

/// The graph message a `[PING from st3] message/ID from …` line announces.
fn ping_id(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix("[PING from st3] ")?
        .split_whitespace()
        .next()
        .filter(|id| id.starts_with("message/"))
}

/// The lines st adds beside a delivery for the agent (st-drivers `ding`): where the person reads
/// replies, and that a message was dictated.
const ST_DELIVERY_NOTES: &[&str] = &[
    "The person reads replies in st, not in the agent's session.",
    "(dictated by voice; it may contain transcription mistakes)",
];

/// `from_harness`, also noting in `delivered` each shown message the harness received: that
/// message is marked delivered instead of announced again.
fn harness_bodies(
    is_user: bool,
    raw: &str,
    shown: &BTreeSet<String>,
    delivered: &mut BTreeSet<String>,
    filters: &[crate::DisplayFilter],
) -> Vec<Body> {
    let mut text = raw.replace("\r\n", "\n");
    // A harness that ran out of context continues from a summary it writes as the person's turn.
    // It is the agent's own notes, kilobytes long: one folded line that opens like a tool call
    // (Nathan, 2026-10-02).
    if is_user && text.contains(COMPACTED) {
        let output = text
            .lines()
            .filter(|line| !self_closing_tag(line))
            .map(str::to_owned)
            .collect();
        return vec![Body::Tool {
            title: "context summary · the conversation was compacted".into(),
            state: ToolState::Ok,
            output,
        }];
    }
    let mut bodies = Vec::new();
    // The messages whose st envelope this prompt carries: a channel delivery around one of them
    // is that message, already read or shown, never a line of its own.
    let mut enveloped = BTreeSet::new();
    // st's own envelope, as codex and the pi family receive it.
    for (block, head) in take_deliveries(&mut text, "smalltalk-message") {
        let from = attribute(&head, "from").unwrap_or_default();
        let subject = attribute(&head, "subject").unwrap_or_default();
        let graph = attribute(&head, "graph").unwrap_or_default();
        enveloped.insert(graph.clone());
        if shown.contains(&graph) {
            delivered.insert(graph);
            continue;
        }
        bodies.push(Body::Mail {
            from,
            to: attribute(&head, "to").unwrap_or_default(),
            subject,
            body: crate::clean_message_text_with_filters(&unescape(&block), filters),
            delivered: false,
            dictated: false,
            signed: None,
            images: Vec::new(),
        });
    }
    // A `<channel>` delivery announces mail that is already in the stream, so it becomes one line.
    for (block, head) in take_deliveries(&mut text, "channel") {
        let sender = attribute(&head, "from").unwrap_or_else(|| "someone".into());
        let message_id = attribute(&head, "messageId");
        // A delivery of mail the stream shows marks that mail delivered; its PING line inside,
        // or the message the channel names (`messageId`), is the same delivery, not another.
        let id = block
            .lines()
            .find_map(ping_id)
            .map(str::to_owned)
            .or(message_id);
        if let Some(id) = id {
            if shown.contains(&id) {
                delivered.insert(id);
                continue;
            }
            // Its st envelope was read above: that is the mail, and what is left of the block
            // is the delivery's own notes (Nathan, 2026-10-03: "delivered to the agent: The
            // person reads replies in st…").
            if enveloped.contains(&id) {
                continue;
            }
        }
        let subject = block
            .lines()
            .find_map(|line| line.trim().strip_prefix("Subject:").map(str::trim))
            .map(str::to_owned)
            .unwrap_or_else(|| {
                shorten(&crate::clean_message_text_with_filters(&block, filters), 70)
            });
        bodies.push(Body::Event(format!(
            "delivered to the agent: {} · from {sender}",
            shorten(&subject, 80)
        )));
    }
    // `[PING from st3] message/ID from SENDER: TITLE` announces mail on its own line.
    let mut kept = Vec::new();
    for line in text.lines() {
        let ping = line
            .trim()
            .strip_prefix("[PING from st3] ")
            .and_then(|rest| rest.split_once(" from "))
            .and_then(|(_, rest)| rest.split_once(": "));
        if let Some(id) = ping_id(line)
            && shown.contains(id)
        {
            delivered.insert(id.to_owned());
            continue;
        }
        match ping {
            Some((sender, title)) => bodies.push(Body::Event(format!(
                "delivered to the agent: {} · from {}",
                shorten(title, 80),
                short(sender)
            ))),
            None => kept.push(line),
        }
    }
    text = kept.join("\n");
    for block in take_blocks(&mut text, "task-notification") {
        let status = field(&block, "status").unwrap_or_else(|| "update".into());
        let summary = field(&block, "summary").unwrap_or_default();
        bodies.push(Body::Event(format!(
            "background task {status}: {}",
            shorten(&summary, 90)
        )));
    }
    for command in take_blocks(&mut text, "command-name") {
        let args = field(raw, "command-args").unwrap_or_default();
        bodies.push(Body::User(
            format!("{} {}", command.trim(), args).trim().to_owned(),
        ));
    }
    for (tag, state) in [
        ("local-command-stdout", ToolState::Ok),
        ("local-command-stderr", ToolState::Failed),
    ] {
        for output in take_blocks(&mut text, tag) {
            bodies.push(Body::Tool {
                title: "command output".into(),
                state,
                output: output.trim().lines().map(str::to_owned).collect(),
            });
        }
    }
    for _ in take_blocks(&mut text, "turn_aborted") {
        bodies.push(Body::Event("the turn was interrupted".into()));
    }
    for reply in take_blocks(&mut text, "send_user_message_question_reply") {
        let answers = serde_json::from_str::<Value>(reply.trim())
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|item| {
                item.get("answer")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        if !answers.is_empty() {
            bodies.push(Body::User(answers.join("\n")));
        }
    }
    if raw.contains("<command-name") {
        take_blocks(&mut text, "command-args"); // already displayed with the parsed command
    }
    // st's own notes beside a delivery tell the agent something; the person never typed them.
    let mut text = text
        .lines()
        .filter(|line| !ST_DELIVERY_NOTES.contains(&line.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    if filters.contains(&crate::DisplayFilter::ContextBlocks) {
        filter_context_blocks(&mut text)
    }
    let rest = crate::clean_message_text_with_filters(&text, filters);
    if !rest.is_empty() {
        if is_user {
            bodies.insert(0, Body::User(rest));
        } else {
            bodies.push(Body::Event(rest));
        }
    }
    bodies
}

fn clock(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn tool_title(name: &str, arguments: &Value) -> String {
    // Codex's code mode passes a script; its commands are what a person wants to see.
    if let Value::String(script) = arguments {
        let commands = script_commands(script);
        return match commands.as_slice() {
            [] => format!("{name} {}", shorten(script, 100)),
            [only] => format!("$ {}", first_line(only)),
            [first, rest @ ..] => format!("$ {} (+{} more)", first_line(first), rest.len()),
        };
    }
    if let Some(ms) = arguments.get("duration_ms").and_then(Value::as_u64) {
        return format!("{name} {}s", ms.div_ceil(1000));
    }
    let pick = [
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "skill",
    ]
    .iter()
    .find_map(|key| arguments.get(key).and_then(Value::as_str));
    match (name, pick) {
        ("Bash" | "bash" | "shell" | "exec_command", Some(command)) => {
            format!("$ {}", first_line(command))
        }
        (_, Some(detail)) => format!("{name} {}", first_line(detail)),
        _ => name.to_owned(),
    }
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or("")
}

/// The shell commands in a codex code-mode script: each `cmd:` string literal it passes to
/// `exec_command`, unescaped.
fn script_commands(script: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut rest = script;
    while let Some(offset) = rest.find("cmd:") {
        rest = rest[offset + 4..].trim_start();
        let Some(quote) = rest
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\'' | '`'))
        else {
            continue;
        };
        let mut command = String::new();
        let mut chars = rest[1..].char_indices();
        let mut end = rest.len();
        while let Some((index, c)) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some((_, 'n')) => command.push('\n'),
                    Some((_, 't')) => command.push('\t'),
                    Some((_, other)) => command.push(other),
                    None => {}
                },
                c if c == quote => {
                    end = index + 2;
                    break;
                }
                c => command.push(c),
            }
        }
        commands.push(command);
        rest = &rest[end.min(rest.len())..];
    }
    commands
}

/// What a tool printed, and whether a command in it failed. Codex's code mode reports each
/// command as JSON (`{"exit_code":…,"output":…}`, inside `{"status":…,"value":…}` when the
/// script awaited several); those become the command's own output.
fn tool_output(content: &Value) -> (Vec<String>, bool) {
    if is_redacted(content) {
        return (vec!["output not recorded".into()], false);
    }
    let mut cut = false;
    let texts: Vec<String> = match content {
        // Some transcripts store the content array as JSON text, which st cuts at 8 KB.
        Value::String(text) => {
            let whole = text.strip_suffix(CUT_MARKER);
            cut = whole.is_some();
            let text = whole.unwrap_or(text);
            match serde_json::from_str::<Value>(text) {
                Ok(inner @ Value::Array(_)) if !cut => return tool_output(&inner),
                _ if text.trim_start().starts_with("[{") => {
                    let mut texts = string_fields(text, "text");
                    texts.extend(std::iter::repeat_n(
                        IMAGE.to_owned(),
                        text.matches("\"type\":\"image\"").count(),
                    ));
                    if texts.is_empty() {
                        vec![text.to_owned()]
                    } else {
                        texts
                    }
                }
                _ => vec![text.to_owned()],
            }
        }
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(Value::as_str) == Some("image") {
                    return Some(IMAGE.to_owned());
                }
                item.get("text")
                    .and_then(Value::as_str)
                    .or(item.as_str())
                    .map(str::to_owned)
            })
            .collect(),
        Value::Null => vec![],
        other => vec![
            other
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| other.to_string()),
        ],
    };
    let mut lines = Vec::new();
    let mut failed = false;
    for text in texts {
        for line in text.lines() {
            let Some(reports) = command_report(line) else {
                // Code mode's own preamble says nothing the reports do not.
                if !matches!(line.trim(), "Script completed" | "Output:")
                    && !line.starts_with("Wall time ")
                {
                    lines.push(line.to_owned());
                }
                continue;
            };
            for (output, exit) in reports {
                if lines
                    .last()
                    .is_some_and(|line: &String| !line.trim().is_empty())
                {
                    lines.push(String::new());
                }
                lines.extend(output.lines().map(str::to_owned));
                if let Some(code) = exit.filter(|code| *code != 0) {
                    failed = true;
                    lines.push(format!("exit {code}"));
                }
            }
        }
    }
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.truncate(400);
    if cut {
        lines.push("… st kept only the start of this output".into());
    }
    (lines, failed)
}

/// How an image a tool returned is drawn in text.
const IMAGE: &str = "[image]";

/// What st appends to a transcript value it cut short.
const CUT_MARKER: &str = "\n[st truncated this native timeline value]";

/// Every `"key":"…"` string in JSON text that may be cut short, decoded; a string the cut
/// ends early keeps what it had.
fn string_fields(json: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\":\"");
    let mut found = Vec::new();
    let mut rest = json;
    while let Some(offset) = rest.find(&needle) {
        rest = &rest[offset + needle.len()..];
        let mut value = String::new();
        let mut chars = rest.char_indices();
        let mut end = rest.len();
        while let Some((index, c)) = chars.next() {
            match c {
                '"' => {
                    end = index + 1;
                    break;
                }
                '\\' => match chars.next().map(|(_, escaped)| escaped) {
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some('r') => {}
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
                        if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            value.push(c);
                        }
                    }
                    Some(other) => value.push(other),
                    None => {}
                },
                c => value.push(c),
            }
        }
        found.push(value);
        rest = &rest[end..];
    }
    found
}

/// One line of codex code-mode output that holds command reports, as each report's output and
/// exit code. Scripts wrap reports as they like (`{"status":"fulfilled","value":…}`,
/// `{"i":0,"result":…}`, arrays); every report inside counts. A line st cut short keeps what
/// its reports printed before the cut.
fn command_report(line: &str) -> Option<Vec<(String, Option<i64>)>> {
    let line = line.trim();
    if !(line.starts_with('{') || line.starts_with('['))
        || ![
            "\"chunk_id\"",
            "\"exit_code\"",
            "\"wall_time_seconds\"",
            "\"status\":\"rejected\"",
        ]
        .iter()
        .any(|key| line.contains(key))
    {
        return None;
    }
    let mut reports = Vec::new();
    match serde_json::from_str::<Value>(line) {
        Ok(value) => collect_reports(&value, &mut reports),
        Err(_) => reports.extend(
            string_fields(line, "output")
                .into_iter()
                .map(|output| (output, None)),
        ),
    }
    (!reports.is_empty()).then_some(reports)
}

fn collect_reports(value: &Value, reports: &mut Vec<(String, Option<i64>)>) {
    match value {
        Value::Object(object) => {
            if let Some(output) = object.get("output").and_then(Value::as_str)
                && ["exit_code", "chunk_id", "wall_time_seconds"]
                    .iter()
                    .any(|key| object.contains_key(*key))
            {
                reports.push((
                    output.to_owned(),
                    object.get("exit_code").and_then(Value::as_i64),
                ));
                return;
            }
            if object.get("status").and_then(Value::as_str) == Some("rejected")
                && let Some(reason) = object.get("reason")
            {
                let text = reason
                    .get("message")
                    .and_then(Value::as_str)
                    .or(reason.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| reason.to_string());
                reports.push((text, Some(1)));
                return;
            }
            for inner in object.values() {
                collect_reports(inner, reports);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_reports(item, reports);
            }
        }
        _ => {}
    }
}

/// A body some recorders keep only as a digest: `{"redacted":true,"sha256":…,"bytes":…}`.
fn is_redacted(value: &Value) -> bool {
    value.get("redacted").and_then(Value::as_bool) == Some(true) && value.get("sha256").is_some()
}

fn short(id: &str) -> String {
    id.trim_start_matches("mission/")
        .trim_start_matches("agent/")
        .to_owned()
}
