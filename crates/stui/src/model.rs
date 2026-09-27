use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use st3_client::{
    Agent, Attention, Client, ClientError, Device, Envelope, ErrorCode, EventPage, EventType,
    Fence, Launch, Machine, Message, Mission, Page, Resource, Runtime, Session, Snapshot,
    TimelineBody, TimelineEntry, TimelineRole, Work,
};

/// Each collection is deliberately capped. The UI shows a truncation marker when a cap is hit.
const PAGE_SIZE: usize = 50;
pub const MAX_PAGES: usize = 4;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Collection {
    pub items: Vec<Resource>,
    pub snapshot: Option<Snapshot>,
    pub truncated: bool,
}

impl Collection {
    pub fn find(&self, id: &str) -> Option<&Resource> {
        self.items.iter().find(|item| item.header().id == id)
    }

    pub fn fence(&self, id: &str) -> Option<Fence> {
        let item = self.find(id)?;
        let mut fence = Fence {
            snapshot_id: self.snapshot.as_ref()?.id.clone(),
            subject_revisions: BTreeMap::from([(id.to_owned(), item.header().revision.clone())]),
            ..Fence::default()
        };
        if let Resource::Runtime(runtime) = item {
            fence.runtime_incarnation = runtime.incarnation_id.clone();
            fence.terminal_sequence = runtime.terminal_sequence;
        }
        Some(fence)
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Model {
    pub now: Collection,
    pub messages: Collection,
    pub launches: Collection,
    pub missions: Collection,
    pub work: Collection,
    pub agents: Collection,
    pub sessions: Collection,
    pub runtimes: Collection,
    pub machines: Collection,
    pub devices: Collection,
    pub timeline: Vec<TimelineEntry>,
    pub timeline_truncated: bool,
    pub event_cursor: String,
    pub actor: String,
    pub status: String,
    recent_events: VecDeque<String>,
    #[serde(skip)]
    pending_refresh: BTreeSet<Kind>,
    #[serde(skip)]
    last_observation_refresh: Option<std::time::Instant>,
    #[serde(skip)]
    last_work_history_refresh: Option<std::time::Instant>,
}

impl Model {
    pub async fn bootstrap(client: &Client) -> Result<Self> {
        let capabilities = client
            .capabilities()
            .await
            .context("client capabilities")?
            .value;
        let mut model = Self {
            event_cursor: capabilities.event_cursor,
            actor: capabilities.session_actor,
            ..Self::default()
        };
        let (now, agents, sessions) = tokio::try_join!(
            read_pages(client, Kind::Now),
            read_pages(client, Kind::Agents),
            read_pages(client, Kind::Sessions),
        )?;
        model.now = now;
        model.agents = agents;
        model.sessions = sessions;
        model.status = "Connected · loading details…".into();
        Ok(model)
    }

    pub async fn reload(&mut self, client: &Client) -> Result<()> {
        let (now, launches, missions, work, agents, sessions, runtimes, machines, devices) = tokio::try_join!(
            read_pages(client, Kind::Now),
            read_pages(client, Kind::Launches),
            read_pages(client, Kind::Missions),
            read_pages(client, Kind::Work),
            read_pages(client, Kind::Agents),
            read_pages(client, Kind::Sessions),
            read_pages(client, Kind::Runtimes),
            read_pages(client, Kind::Machines),
            read_pages(client, Kind::Devices),
        )?;
        (
            self.now,
            self.launches,
            self.missions,
            self.work,
            self.agents,
            self.sessions,
            self.runtimes,
            self.machines,
            self.devices,
        ) = (
            now, launches, missions, work, agents, sessions, runtimes, machines, devices,
        );
        self.last_work_history_refresh = Some(std::time::Instant::now());
        self.status = "Connected".into();
        Ok(())
    }

    pub async fn sync(&mut self, client: &Client) -> Result<(bool, Vec<String>, bool)> {
        let response = client
            .events(Some(&self.event_cursor), Some(PAGE_SIZE), Some(15_000))
            .await;
        let events = match response {
            Ok(envelope) => envelope.value,
            Err(ClientError::Api(ErrorCode::CursorGap, _, _)) => {
                // A cursor gap invalidates every cached projection and timeline.
                let caps = client.capabilities().await?.value;
                self.event_cursor = caps.event_cursor;
                self.timeline.clear();
                self.recent_events.clear();
                self.reload(client).await?;
                self.status = "Resynchronized after cursor gap".into();
                return Ok((true, Vec::new(), true));
            }
            Err(error) => return Err(error.into()),
        };
        let (mut changed, mut invalidated_sessions) = self.consume_events(events);
        // Coalesce a busy event stream before projection reads. A mission
        // publish/start can emit dozens of related changes; refreshing after
        // each event makes one Chat viewer multiply daemon CPU under load.
        if changed {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            for _ in 0..16 {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let more = client
                    .events(
                        Some(&self.event_cursor),
                        Some(PAGE_SIZE),
                        Some(remaining.as_millis().min(1_500) as u64),
                    )
                    .await?
                    .value;
                if more.items.is_empty() {
                    break;
                }
                let (more_changed, sessions) = self.consume_events(more);
                changed |= more_changed;
                invalidated_sessions.extend(sessions);
            }
        }
        if changed {
            self.reload_changed(client).await?;
        }
        invalidated_sessions.sort();
        invalidated_sessions.dedup();
        Ok((changed, invalidated_sessions, false))
    }

    async fn reload_changed(&mut self, client: &Client) -> Result<()> {
        let mut remaining = std::mem::take(&mut self.pending_refresh);
        for kind in remaining.clone() {
            let full_work_history = kind != Kind::Work
                || self
                    .last_work_history_refresh
                    .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(30));
            let collection = match read_pages_mode(client, kind, full_work_history).await {
                Ok(collection) => collection,
                Err(error) => {
                    self.pending_refresh.extend(remaining);
                    return Err(error);
                }
            };
            remaining.remove(&kind);
            match kind {
                Kind::Now => self.now = collection,
                Kind::Launches => self.launches = collection,
                Kind::Missions => self.missions = collection,
                Kind::Work => {
                    if full_work_history {
                        self.work = collection;
                        self.last_work_history_refresh = Some(std::time::Instant::now());
                    } else {
                        self.work = with_cached_work_history(collection, &self.work);
                    }
                }
                Kind::Agents => self.agents = collection,
                Kind::Sessions => self.sessions = collection,
                Kind::Runtimes => self.runtimes = collection,
                Kind::Machines => self.machines = collection,
                Kind::Devices => self.devices = collection,
                Kind::NativeSessions => unreachable!("native discovery has its own refresh"),
            }
        }
        Ok(())
    }

    /// Native harnesses may start outside st3, so no graph event announces them.
    pub async fn refresh_sessions(&mut self, client: &Client) -> Result<bool> {
        let native = read_pages(client, Kind::NativeSessions).await?;
        let mut next = self.sessions.clone();
        next.items.retain(|item| !matches!(item, Resource::Session(session) if session.extra.get("managed") == Some(&serde_json::Value::Bool(false))));
        next.items.extend(native.items);
        next.truncated |= native.truncated;
        let changed =
            self.sessions.items != next.items || self.sessions.truncated != next.truncated;
        self.sessions = next;
        Ok(changed)
    }

    fn consume_events(&mut self, events: EventPage) -> (bool, Vec<String>) {
        if events.items.is_empty() {
            self.event_cursor = events.resume_cursor;
            return (false, Vec::new());
        }
        let mut changed = false;
        let mut invalidated_sessions = Vec::new();
        for event in events.items {
            if self.recent_events.contains(&event.id) {
                self.event_cursor = event.next_cursor;
                continue;
            }
            self.recent_events.push_back(event.id);
            if self.recent_events.len() > 256 {
                self.recent_events.pop_front();
            }
            let change = event.body.get("change").and_then(serde_json::Value::as_str);
            if matches!(change, Some("work.renewed" | "harness.usage")) {
                self.event_cursor = event.next_cursor;
                continue;
            }
            if change == Some("harness.observed") {
                if self
                    .last_observation_refresh
                    .is_some_and(|last| last.elapsed() < std::time::Duration::from_secs(30))
                {
                    self.event_cursor = event.next_cursor;
                    continue;
                }
                self.last_observation_refresh = Some(std::time::Instant::now());
            }
            if matches!(event.event_type, EventType::CapabilitiesChanged) {
                self.pending_refresh.extend(Kind::ALL);
            } else if matches!(event.event_type, EventType::TerminalAvailable) {
                self.pending_refresh.insert(Kind::Runtimes);
            } else if matches!(
                event.event_type,
                EventType::Upsert | EventType::Delete | EventType::TimelineDelta
            ) {
                for id in &event.resource_ids {
                    if !(id.starts_with("session/")
                        && event.body.get("reason").and_then(serde_json::Value::as_str)
                            == Some("session-timeline-invalidated"))
                    {
                        self.pending_refresh.extend(Kind::for_resource(id));
                    }
                }
            }
            changed = !self.pending_refresh.is_empty();
            if event.body.get("reason").and_then(serde_json::Value::as_str)
                == Some("session-timeline-invalidated")
            {
                invalidated_sessions.extend(
                    event
                        .resource_ids
                        .iter()
                        .filter(|id| id.starts_with("session/"))
                        .cloned(),
                );
            }
            self.event_cursor = event.next_cursor;
        }
        invalidated_sessions.sort();
        invalidated_sessions.dedup();
        (changed, invalidated_sessions)
    }

    pub async fn load_timeline_with_pages(
        &mut self,
        client: &Client,
        session_id: &str,
        page_limit: usize,
    ) -> Result<()> {
        for attempt in 0..3 {
            match self
                .load_timeline_once(client, session_id, page_limit)
                .await
            {
                Ok(()) => return Ok(()),
                Err(error)
                    if attempt < 2
                        && error.downcast_ref::<ClientError>().is_some_and(|error| {
                            matches!(error, ClientError::Api(ErrorCode::PageCursorExpired, _, _))
                        }) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded timeline retry returns from every attempt")
    }

    async fn load_timeline_once(
        &mut self,
        client: &Client,
        session_id: &str,
        page_limit: usize,
    ) -> Result<()> {
        let mut cursor = None;
        let mut entries = Vec::new();
        let mut has_more = false;
        for _ in 0..page_limit.clamp(1, 32) {
            let page = client
                .timeline(session_id, cursor.as_deref(), Some(PAGE_SIZE))
                .await?
                .value;
            has_more = page.page.has_more;
            cursor = page.page.next_cursor;
            entries.extend(page.items);
            if !has_more
                || (page_limit <= MAX_PAGES && !conversation_needs_older_page(&entries, has_more))
            {
                break;
            }
            if cursor.is_none() {
                anyhow::bail!("timeline page omitted continuation cursor");
            }
        }
        self.timeline = entries;
        self.timeline_truncated = has_more;
        self.timeline.sort_by_key(|entry| entry.sequence);
        Ok(())
    }

    pub async fn load_messages_for_peer(&mut self, client: &Client, peer: &str) -> Result<()> {
        let Envelope {
            snapshot, value, ..
        } = client
            .trusted_unscoped_read()
            .messages_list_for_peer(peer, None, Some(PAGE_SIZE), true)
            .await?;
        self.messages = Collection {
            items: value.items,
            snapshot: Some(snapshot),
            truncated: value.page.has_more,
        };
        Ok(())
    }

    pub fn attention(&self) -> impl Iterator<Item = &Attention> {
        self.now.items.iter().filter_map(|item| match item {
            Resource::Attention(v) if v.state != "resolved" && v.person_id == self.actor => Some(v),
            _ => None,
        })
    }
    pub fn agents(&self) -> impl Iterator<Item = &Agent> {
        self.agents.items.iter().filter_map(|item| match item {
            Resource::Agent(v) => Some(v),
            _ => None,
        })
    }
    pub fn undeclared_sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.items.iter().filter_map(|item| match item {
            Resource::Session(v)
                if v.state == "running"
                    && v.extra.get("managed").and_then(serde_json::Value::as_bool)
                        == Some(false) =>
            {
                Some(v)
            }
            _ => None,
        })
    }
    pub fn messages(&self, _session: Option<&str>, peer: &str) -> impl Iterator<Item = &Message> {
        self.messages
            .items
            .iter()
            .filter_map(move |item| match item {
                Resource::Message(v) if v.from == peer || v.to == peer => Some(v),
                _ => None,
            })
    }
    pub fn launches(&self) -> impl Iterator<Item = &Launch> {
        self.launches.items.iter().filter_map(|item| match item {
            Resource::Launch(v) => Some(v),
            _ => None,
        })
    }
    pub fn missions(&self) -> impl Iterator<Item = &Mission> {
        self.missions.items.iter().filter_map(|item| match item {
            Resource::Mission(v) => Some(v),
            _ => None,
        })
    }
    pub fn work(&self) -> impl Iterator<Item = &Work> {
        self.work.items.iter().filter_map(|item| match item {
            Resource::Work(v) => Some(v),
            _ => None,
        })
    }
    pub fn runtimes(&self) -> impl Iterator<Item = &Runtime> {
        self.runtimes.items.iter().filter_map(|item| match item {
            Resource::Runtime(v) => Some(v),
            _ => None,
        })
    }
    pub fn machines(&self) -> impl Iterator<Item = &Machine> {
        self.machines.items.iter().filter_map(|item| match item {
            Resource::Machine(v) => Some(v),
            _ => None,
        })
    }
    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.devices.items.iter().filter_map(|item| match item {
            Resource::Device(v) => Some(v),
            _ => None,
        })
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum Kind {
    Now,
    Launches,
    Missions,
    Work,
    Agents,
    Sessions,
    NativeSessions,
    Runtimes,
    Machines,
    Devices,
}

impl Kind {
    const ALL: [Self; 9] = [
        Self::Now,
        Self::Launches,
        Self::Missions,
        Self::Work,
        Self::Agents,
        Self::Sessions,
        Self::Runtimes,
        Self::Machines,
        Self::Devices,
    ];
    fn for_resource(id: &str) -> &'static [Self] {
        if id.starts_with("attention/") {
            &[Self::Now]
        } else if id.starts_with("message/") {
            &[Self::Now]
        } else if id.starts_with("launch/") {
            &[Self::Launches, Self::Now]
        } else if id.starts_with("mission/") || id.starts_with("mission-run/") {
            &[Self::Missions, Self::Work, Self::Now]
        } else if id.starts_with("step-run/") {
            &[Self::Work, Self::Agents, Self::Missions, Self::Now]
        } else if id.starts_with("agent/") {
            &[Self::Agents, Self::Runtimes]
        } else if id.starts_with("session/") {
            &[Self::Sessions]
        } else if id.starts_with("runtime/") {
            &[Self::Runtimes, Self::Agents, Self::Machines]
        } else if id.starts_with("machine/") || id.starts_with("host/") {
            &[Self::Machines]
        } else if id.starts_with("device/") {
            &[Self::Devices]
        } else {
            &[]
        }
    }
}

async fn read_pages(client: &Client, kind: Kind) -> Result<Collection> {
    read_pages_mode(client, kind, true).await
}

async fn read_pages_mode(
    client: &Client,
    kind: Kind,
    include_work_history: bool,
) -> Result<Collection> {
    for attempt in 0..3 {
        match read_pages_once(client, kind, include_work_history).await {
            Ok(collection) => return Ok(collection),
            Err(error)
                if attempt < 2
                    && error.downcast_ref::<ClientError>().is_some_and(|error| {
                        matches!(error, ClientError::Api(ErrorCode::PageCursorExpired, _, _))
                    }) =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("bounded page retry returns from every attempt")
}

async fn read_pages_once(
    client: &Client,
    kind: Kind,
    include_work_history: bool,
) -> Result<Collection> {
    if kind == Kind::Work {
        let mut current = read_pages_once_inner(client, kind, false).await?;
        if !include_work_history {
            return Ok(current);
        }
        let history = read_pages_once_inner(client, kind, true).await?;
        let mut seen = current
            .items
            .iter()
            .map(|item| item.header().id.clone())
            .collect::<BTreeSet<_>>();
        current.items.extend(
            history
                .items
                .into_iter()
                .filter(|item| seen.insert(item.header().id.clone())),
        );
        current.truncated |= history.truncated;
        return Ok(current);
    }
    read_pages_once_inner(client, kind, false).await
}

fn with_cached_work_history(mut current: Collection, previous: &Collection) -> Collection {
    let mut seen = current
        .items
        .iter()
        .map(|item| item.header().id.clone())
        .collect::<BTreeSet<_>>();
    current
        .items
        .extend(previous.items.iter().filter_map(|item| {
            (item
                .header()
                .operational
                .as_ref()
                .is_some_and(|op| op.layer == "history")
                && seen.insert(item.header().id.clone()))
            .then(|| item.clone())
        }));
    current.truncated |= previous.truncated;
    current
}

async fn read_pages_once_inner(
    client: &Client,
    kind: Kind,
    work_history: bool,
) -> Result<Collection> {
    let mut result = Collection::default();
    let mut cursor = None;
    for page_index in 0..MAX_PAGES {
        let Envelope {
            snapshot, value, ..
        }: Envelope<Page> = match kind {
            Kind::Now => {
                client
                    .attention_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Launches => {
                client
                    .launches_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Missions => {
                client
                    .missions_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Work => {
                client
                    .work_list(cursor.as_deref(), Some(PAGE_SIZE), work_history)
                    .await?
            }
            Kind::Agents => {
                client
                    .agents_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Sessions => {
                client
                    .sessions_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::NativeSessions => {
                client
                    .sessions_list_native(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Runtimes => {
                client
                    .runtimes_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Machines => {
                client
                    .machines_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
            Kind::Devices => {
                client
                    .devices_list(cursor.as_deref(), Some(PAGE_SIZE), false)
                    .await?
            }
        };
        result.snapshot = Some(snapshot);
        result.items.extend(value.items);
        if !value.page.has_more {
            return Ok(result);
        }
        if page_index + 1 == MAX_PAGES {
            result.truncated = true;
            return Ok(result);
        }
        cursor = value.page.next_cursor;
        if cursor.is_none() {
            anyhow::bail!("resource page omitted continuation cursor");
        }
    }
    Ok(result)
}

pub fn timeline_line(entry: &TimelineEntry) -> String {
    let role = format!("{:?}", entry.role).to_lowercase();
    let body = match &entry.body {
        TimelineBody::Content(v) => v
            .text
            .as_deref()
            .map(|text| {
                let text = clean_message_text(text);
                if entry.role == TimelineRole::Tool {
                    markdown_like(&readable_tool_text(&text))
                } else {
                    markdown_like(&text)
                }
            })
            .unwrap_or_else(|| format!("[{} attachment]", v.media_type)),
        TimelineBody::ToolCall(v) => format!("called {}", v.name),
        TimelineBody::ToolResult(v) => format!("tool result: {:?}", v.status),
        TimelineBody::Status(v) => format!("status: {:?}", v.status),
        TimelineBody::Error(v) => format!("error: {}", v.message),
        TimelineBody::Redaction(_) => "[redacted]".into(),
        TimelineBody::Truncation(_) => "[truncated]".into(),
        TimelineBody::Usage(_) => "[usage]".into(),
        TimelineBody::Message(_) => "[message]".into(),
        TimelineBody::Unknown { entry_type, .. } => format!("[{entry_type}]"),
    };
    format!("{role}: {body}")
}

fn readable_tool_text(text: &str) -> String {
    let (first, rest) = text.split_once("\n\n\n").unwrap_or((text, ""));
    if first.len() > 2_048 || !first.trim_start().starts_with('{') {
        return text.to_owned();
    }
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(first) else {
        return text.to_owned();
    };
    if fields.is_empty() || fields.len() > 12 {
        return text.to_owned();
    }
    let mut lines = fields
        .iter()
        .map(|(key, value)| {
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            format!("- {key}: {value}")
        })
        .collect::<Vec<_>>();
    if !rest.trim().is_empty() {
        lines.push(rest.trim().to_owned());
    }
    lines.join("\n")
}

pub fn conversation_needs_older_page(entries: &[TimelineEntry], has_more: bool) -> bool {
    has_more
        && entries
            .iter()
            .filter(|entry| match &entry.body {
                TimelineBody::Content(content) => content
                    .text
                    .as_deref()
                    .is_none_or(|text| !clean_message_text(text).is_empty()),
                _ => false,
            })
            .count()
            < 12
}

pub fn clean_message_text(raw: &str) -> String {
    let normalized = raw.replace("\r\n", "\n");
    let mut output = String::new();
    let mut plain = String::new();
    let mut code = false;
    for line in normalized.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            if !code {
                output.push_str(&strip_internal_markup(&plain));
                plain.clear();
            }
            output.push_str(line);
            code = !code;
        } else if code {
            output.push_str(line);
        } else {
            plain.push_str(line);
        }
    }
    output.push_str(&strip_internal_markup(&plain));
    let safe = output
        .chars()
        .filter(|character| *character == '\n' || *character == '\t' || !character.is_control())
        .collect::<String>();
    let text = safe
        .trim()
        .strip_prefix("[PING] ?")
        .unwrap_or(safe.trim())
        .trim();
    if let Some(start) = text.rfind(" [id:message/")
        && text.ends_with(']')
    {
        text[..start].trim_end().to_owned()
    } else {
        text.to_owned()
    }
}
fn strip_internal_markup(input: &str) -> String {
    let mut in_st3_channel = false;
    let mut text = input
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("<channel ")
                && trimmed.contains("source=\"plugin:st3-channel:st3\"")
                && trimmed.ends_with('>')
            {
                in_st3_channel = true;
                return None;
            }
            if in_st3_channel && trimmed == "</channel>" {
                in_st3_channel = false;
                return None;
            }
            if trimmed.starts_with("[st3-delivery:") && trimmed.ends_with(".md]") {
                return None;
            }
            Some(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if input.ends_with('\n') && !text.is_empty() {
        text.push('\n');
    }
    for tag in [
        "analysis",
        "thinking",
        "think",
        "internal",
        "system-reminder",
        "function_calls",
        "tool_result",
    ] {
        loop {
            let lower = text.to_ascii_lowercase();
            let Some(start) = lower.find(&format!("<{tag}")) else {
                break;
            };
            let Some(open_end) = lower[start..].find('>').map(|offset| start + offset + 1) else {
                text.truncate(start);
                break;
            };
            let Some(close) = lower[open_end..].find(&format!("</{tag}>")) else {
                text.truncate(start);
                break;
            };
            text.replace_range(start..open_end + close + tag.len() + 3, "");
        }
    }
    loop {
        let Some(start) = text.find("<|im_start|>") else {
            break;
        };
        let Some(separator) = text[start..]
            .find("<|im_sep|>")
            .map(|offset| start + offset)
        else {
            text.truncate(start);
            break;
        };
        let header = &text[start..separator];
        let hidden =
            header.contains("<|meta_sep|>analysis") || header.contains("<|meta_sep|>commentary");
        let body_start = separator + "<|im_sep|>".len();
        if hidden {
            let end = text[body_start..]
                .find("<|im_end|>")
                .map(|offset| body_start + offset + "<|im_end|>".len())
                .unwrap_or(text.len());
            text.replace_range(start..end, "");
        } else {
            text.replace_range(start..body_start, "");
        }
    }
    for token in ["<|im_end|>", "<|fim_suffix|>", "<|im_sep|>"] {
        text = text.replace(token, "");
    }
    text
}
fn markdown_like(input: &str) -> String {
    let mut code = false;
    input
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if let Some(language) = trimmed.strip_prefix("```") {
                let was_code = code;
                code = !code;
                if was_code {
                    "└─".to_owned()
                } else {
                    format!("┌─ {}", language.trim())
                }
            } else if code {
                format!("  {line}")
            } else if let Some(heading) = trimmed.strip_prefix('#') {
                format!("▌ {}", heading.trim_start_matches('#').trim())
            } else if let Some(item) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
            {
                format!("• {}", item.replace("**", "").replace('`', ""))
            } else if let Some(quote) = trimmed.strip_prefix("> ") {
                format!("│ {}", quote.replace("**", "").replace('`', ""))
            } else {
                line.replace("**", "").replace('`', "")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_resource(id: &str, state: &str, layer: &str) -> Resource {
        serde_json::from_value(serde_json::json!({
            "kind":"work", "id":id, "revision":"one", "updated_at":"2026-09-25T08:00:00Z",
            "operational":{"layer":layer,"actionable":layer=="current","reasons":[]},
            "mission_run_id":"mission-run/test", "generation_id":"run-generation/test",
            "definition_id":"definition/test", "path":"step", "state":state,
            "attempt":1, "readiness_epoch":1, "claimant":null, "claim_incarnation":null,
            "blocked_reason":null, "blockers":[], "goals":[], "constraints":[]
        }))
        .unwrap()
    }

    #[test]
    fn current_work_refresh_preserves_history_without_stale_active_work() {
        let previous = Collection {
            items: vec![
                work_resource("step-run/old", "claimed", "current"),
                work_resource("step-run/done", "completed", "history"),
            ],
            truncated: true,
            ..Collection::default()
        };
        let current = Collection {
            items: vec![work_resource("step-run/new", "ready", "current")],
            ..Collection::default()
        };
        let merged = with_cached_work_history(current, &previous);
        assert_eq!(merged.items.len(), 2);
        assert!(
            merged
                .items
                .iter()
                .any(|item| item.header().id == "step-run/new")
        );
        assert!(
            merged
                .items
                .iter()
                .any(|item| item.header().id == "step-run/done")
        );
        assert!(
            !merged
                .items
                .iter()
                .any(|item| item.header().id == "step-run/old")
        );
        assert!(merged.truncated);
    }

    #[test]
    fn regression_recent_messages_include_closed_and_sessionless_messages() {
        let message: Resource = serde_json::from_str(r#"{"kind":"message","id":"message/old","revision":"one","updated_at":"2026-09-25T08:00:00Z","from":"agent/cos","to":"agent/st3","title":null,"content":"hello","state":"closed","sent_at":"2026-09-25T08:00:00Z","in_reply_to":null,"session_id":"session/old"}"#).unwrap();
        let model = Model {
            actor: "person/nathan".into(),
            messages: Collection {
                items: vec![message],
                ..Collection::default()
            },
            ..Model::default()
        };
        assert_eq!(model.messages(Some("session/new"), "agent/st3").count(), 1);
    }

    #[test]
    fn regression_chat_page_skips_status_heartbeats() {
        let statuses = (0..50).map(|sequence| serde_json::from_value::<TimelineEntry>(serde_json::json!({"id":format!("timeline/{sequence}"),"sequence":sequence,"revision":1,"timestamp":"2026-09-25T08:00:00Z","role":"system","type":"status","final":true,"body":{"status":"running","detail":"ready"}})).unwrap()).collect::<Vec<_>>();
        assert!(conversation_needs_older_page(&statuses, true));
        let hidden: TimelineEntry = serde_json::from_str(r#"{"id":"timeline/hidden","sequence":100,"revision":1,"timestamp":"2026-09-25T08:00:00Z","role":"assistant","type":"content","final":true,"body":{"media_type":"text/plain","text":"<thinking>private</thinking>"}}"#).unwrap();
        assert!(conversation_needs_older_page(&[hidden], true));
    }

    #[test]
    fn regression_notification_text_is_cleaned() {
        assert_eq!(
            clean_message_text("[PING] ? hello [id:message/abc]"),
            "hello"
        );
        assert_eq!(clean_message_text("[PING] ?"), "");
    }

    #[test]
    fn omp_tool_json_reads_as_fields_with_timing_preserved() {
        let entry: TimelineEntry = serde_json::from_value(serde_json::json!({
            "id":"timeline/tool", "sequence":1, "revision":1,
            "timestamp":"2026-09-25T08:00:00Z", "role":"tool",
            "type":"content", "final":true,
            "body":{"media_type":"text/plain","text":"{\"presence\":null,\"state\":\"running\"}\n\n\nWall time: 0.03 seconds"}
        })).unwrap();
        let line = timeline_line(&entry);
        assert!(line.contains("• presence: null"));
        assert!(line.contains("• state: running"));
        assert!(line.contains("Wall time: 0.03 seconds"));
        assert!(!line.contains("{\"presence\""));
    }
    #[test]
    fn driver_fixtures_hide_internal_markup_preserve_lines_and_order() {
        let fixtures: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/driver_conversations.json"))
                .unwrap();
        for fixture in fixtures {
            let driver = fixture["driver"].as_str().unwrap();
            let mut entries = fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| {
                    let sequence = message["sequence"].as_u64().unwrap();
                    let entry: TimelineEntry = serde_json::from_value(serde_json::json!({
                        "id":format!("timeline/{driver}/{sequence}"), "sequence":sequence,
                        "revision":1, "timestamp":"2026-09-25T08:00:00Z",
                        "role":message["role"], "type":"content", "final":true,
                        "body":{"media_type":"text/plain", "text":message["text"]}
                    }))
                    .unwrap();
                    (entry, message["expected"].as_str().unwrap().to_owned())
                })
                .collect::<Vec<_>>();
            entries.sort_by_key(|(entry, _)| entry.sequence);
            assert!(
                entries[0].0.sequence < entries[1].0.sequence,
                "{driver} ordering"
            );
            for (entry, expected) in entries {
                assert_eq!(timeline_line(&entry), expected, "{driver}");
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires a live local st3 daemon"]
    async fn live_full_snapshot_latency() {
        let path =
            st3_client::discover_unix_endpoint(std::env::var_os("ST3_ENDPOINT").map(Into::into))
                .unwrap();
        let actor =
            std::env::var("ST3_PERSON").expect("ST3_PERSON selects the person for this live test");
        let client = Client::unix_as(path, actor);
        let started = std::time::Instant::now();
        let mut model = Model::bootstrap(&client).await.unwrap();
        model.reload(&client).await.unwrap();
        eprintln!(
            "full snapshot: {:?}, agents: {}, sessions: {}, work: {} (truncated: {})",
            started.elapsed(),
            model.agents().count(),
            model.sessions.items.len(),
            model.work.items.len(),
            model.work.truncated
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[tokio::test]
    #[ignore = "requires a live local st3 daemon"]
    async fn live_bootstrap_latency() {
        let path =
            st3_client::discover_unix_endpoint(std::env::var_os("ST3_ENDPOINT").map(Into::into))
                .unwrap();
        let actor =
            std::env::var("ST3_PERSON").expect("ST3_PERSON selects the person for this live test");
        let client = Client::unix_as(path, actor);
        let started = std::time::Instant::now();
        let model = Model::bootstrap(&client).await.unwrap();
        eprintln!(
            "bootstrap: {:?}, attention: {}, agents: {}",
            started.elapsed(),
            model.attention().count(),
            model.agents().count()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }

    #[test]
    fn fixture_only_routes_person_attention_to_now() {
        let resources: Vec<Resource> = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/fixtures/resources.json"
        ))
        .unwrap();
        let model = Model {
            actor: "person/nathan".into(),
            now: Collection {
                items: resources,
                ..Collection::default()
            },
            ..Model::default()
        };
        let attention: Vec<_> = model.attention().collect();
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].header.id, "attention/release-review");
    }

    #[test]
    fn action_fence_uses_exact_resource_revision_and_snapshot() {
        let resource: Resource = serde_json::from_str(
            r#"{"id":"attention/one","kind":"attention","revision":"rev-7","updated_at":"2026-09-20T11:00:00Z","attention_kind":"review","source_id":"launch/one","person_id":"person/nathan","title":"Review","detail":"Choose","priority":"high","state":"open","requested_at":"2026-09-20T11:00:00Z"}"#
        ).unwrap();
        let collection = Collection {
            items: vec![resource],
            snapshot: Some(Snapshot {
                id: "snapshot/one".into(),
                host_id: "host/one".into(),
                store_index: 3,
                projection_version: "client-projection.v0".into(),
                created_at: "2026-09-20T11:00:00Z".into(),
            }),
            truncated: false,
        };
        let fence = collection.fence("attention/one").unwrap();
        assert_eq!(fence.snapshot_id, "snapshot/one");
        assert_eq!(fence.subject_revisions["attention/one"], "rev-7");
        assert!(collection.fence("attention/missing").is_none());
    }

    #[test]
    fn fixture_event_replay_does_not_trigger_another_reload() {
        let envelope: Envelope<EventPage> = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/fixtures/events.json"
        ))
        .unwrap();
        let mut model = Model::default();
        assert!(model.consume_events(envelope.value.clone()).0);
        let cursor = model.event_cursor.clone();
        assert!(!model.consume_events(envelope.value).0);
        assert_eq!(model.event_cursor, cursor);
    }

    #[test]
    fn event_refresh_is_selective_and_ignores_unscoped_noise() {
        let events: EventPage = serde_json::from_value(serde_json::json!({
            "kind":"event-page", "oldest_cursor":"event-cursor/node/0", "resume_cursor":"event-cursor/node/2", "has_more":false,
            "items":[
                {"id":"event/noise","epoch":"node","sequence":1,"previous_cursor":"event-cursor/node/0","next_cursor":"event-cursor/node/1","timestamp":"2026-09-25T08:00:00Z","type":"upsert","resource_ids":[],"snapshot_id":"snapshot/one","body":{}},
                {"id":"event/attention","epoch":"node","sequence":2,"previous_cursor":"event-cursor/node/1","next_cursor":"event-cursor/node/2","timestamp":"2026-09-25T08:00:01Z","type":"upsert","resource_ids":["attention/one"],"snapshot_id":"snapshot/two","body":{}}
            ]
        })).unwrap();
        let mut model = Model::default();
        assert!(model.consume_events(events).0);
        assert_eq!(model.pending_refresh, BTreeSet::from([Kind::Now]));
    }

    #[test]
    fn only_a_changed_session_invalidates_its_visible_timeline() {
        let events: EventPage = serde_json::from_value(serde_json::json!({
            "kind":"event-page",
            "oldest_cursor":"event-cursor/node/0",
            "resume_cursor":"event-cursor/node/2",
            "items":[{
                "id":"event/one", "epoch":"node", "sequence":1,
                "previous_cursor":"event-cursor/node/0", "next_cursor":"event-cursor/node/1",
                "timestamp":"2026-09-24T15:00:00Z", "type":"upsert",
                "resource_ids":["session/current"], "snapshot_id":"snapshot/one",
                "body":{"reason":"session-timeline-invalidated"}
            }, {
                "id":"event/two", "epoch":"node", "sequence":2,
                "previous_cursor":"event-cursor/node/1", "next_cursor":"event-cursor/node/2",
                "timestamp":"2026-09-24T15:00:01Z", "type":"upsert",
                "resource_ids":["agent/unrelated"], "snapshot_id":"snapshot/two",
                "body":{"reason":"client-projection-invalidated"}
            }],
            "has_more":false
        }))
        .unwrap();
        let mut model = Model::default();
        let (changed, sessions) = model.consume_events(events);
        assert!(changed);
        assert_eq!(sessions, vec!["session/current"]);
    }

    #[test]
    fn session_only_timeline_event_does_not_reload_fleet_projections() {
        let events: EventPage = serde_json::from_value(serde_json::json!({
            "kind":"event-page", "oldest_cursor":"event-cursor/node/0",
            "resume_cursor":"event-cursor/node/1", "has_more":false,
            "items":[{"id":"event/one", "epoch":"node", "sequence":1,
                "previous_cursor":"event-cursor/node/0", "next_cursor":"event-cursor/node/1",
                "timestamp":"2026-09-24T15:00:00Z", "type":"upsert",
                "resource_ids":["session/current"], "snapshot_id":"snapshot/one",
                "body":{"reason":"session-timeline-invalidated"}}]
        }))
        .unwrap();
        let mut model = Model::default();
        let (changed, sessions) = model.consume_events(events);
        assert!(!changed);
        assert_eq!(sessions, vec!["session/current"]);
    }

    #[test]
    fn lease_renewal_does_not_reload_fleet_projections() {
        let events: EventPage = serde_json::from_value(serde_json::json!({
            "kind":"event-page", "oldest_cursor":"event-cursor/node/0",
            "resume_cursor":"event-cursor/node/1", "has_more":false,
            "items":[{"id":"event/renewal", "epoch":"node", "sequence":1,
                "previous_cursor":"event-cursor/node/0", "next_cursor":"event-cursor/node/1",
                "timestamp":"2026-09-24T15:00:00Z", "type":"upsert",
                "resource_ids":["step-run/one"], "snapshot_id":"snapshot/one",
                "body":{"change":"work.renewed","state":"working"}}]
        }))
        .unwrap();
        let mut model = Model::default();
        assert!(!model.consume_events(events).0);
        assert_eq!(model.event_cursor, "event-cursor/node/1");
    }

    #[test]
    fn observation_burst_refreshes_roster_once() {
        let events: EventPage = serde_json::from_value(serde_json::json!({
            "kind":"event-page", "oldest_cursor":"event-cursor/node/0",
            "resume_cursor":"event-cursor/node/2", "has_more":false,
            "items":[
                {"id":"event/one","epoch":"node","sequence":1,"previous_cursor":"event-cursor/node/0","next_cursor":"event-cursor/node/1","timestamp":"2026-09-25T08:00:00Z","type":"upsert","resource_ids":["agent/one"],"snapshot_id":"snapshot/one","body":{"change":"harness.observed"}},
                {"id":"event/two","epoch":"node","sequence":2,"previous_cursor":"event-cursor/node/1","next_cursor":"event-cursor/node/2","timestamp":"2026-09-25T08:00:01Z","type":"upsert","resource_ids":["agent/two"],"snapshot_id":"snapshot/two","body":{"change":"harness.observed"}}
            ]
        })).unwrap();
        let mut model = Model::default();
        assert!(model.consume_events(events.clone()).0);
        assert_eq!(
            model.pending_refresh,
            BTreeSet::from([Kind::Agents, Kind::Runtimes])
        );
        model.pending_refresh.clear();
        let mut another = events;
        another.items[0].id = "event/three".into();
        another.items[0].next_cursor = "event-cursor/node/3".into();
        another.items.truncate(1);
        assert!(!model.consume_events(another).0);
        assert!(model.pending_refresh.is_empty());
    }

    #[test]
    fn running_undeclared_sessions_are_separate_from_managed_and_history() {
        let resources: Vec<Resource> = serde_json::from_str(
            r#"[
                {"kind":"session","id":"session/external","revision":"one","updated_at":"2026-09-24T09:00:00Z","owner_id":"external-session/codex/one","state":"running","started_at":"2026-09-24T08:00:00Z","ended_at":null,"timeline_cursor":"cursor/one","managed":false,"driver":"codex","native_session_id":"one"},
                {"kind":"session","id":"session/unresolved","revision":"two","updated_at":"2026-09-24T09:00:00Z","owner_id":"external-process/claude/42","state":"running","started_at":"2026-09-24T08:00:00Z","ended_at":null,"timeline_cursor":"cursor/two","managed":false,"driver":"claude","native_session_id":null},
                {"kind":"session","id":"session/managed","revision":"three","updated_at":"2026-09-24T09:00:00Z","owner_id":"agent/one","state":"running","started_at":"2026-09-24T08:00:00Z","ended_at":null,"timeline_cursor":"cursor/three"},
                {"kind":"session","id":"session/old","revision":"four","updated_at":"2026-09-24T09:00:00Z","owner_id":"external-session/codex/old","state":"completed","started_at":"2026-09-23T08:00:00Z","ended_at":"2026-09-23T09:00:00Z","timeline_cursor":"cursor/four","managed":false}
            ]"#,
        )
        .unwrap();
        let model = Model {
            sessions: Collection {
                items: resources,
                ..Collection::default()
            },
            ..Model::default()
        };
        let ids = model
            .undeclared_sessions()
            .map(|session| session.header.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["session/external", "session/unresolved"]);
    }
}
