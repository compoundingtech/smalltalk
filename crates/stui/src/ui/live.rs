//! `stui`: the screens on the live graph.
//!
//! The feed keeps the attention, missions and agents windows current over one socket, joined
//! by st, and follows the open terminal on the same socket. This loop turns those windows
//! into a `World`, fetches what only the selected item or tab needs (a conversation, a launch
//! preview, the fleet), and carries out the actions the screens queue. A refresh replaces
//! data in place: it never empties a list or a conversation while the fresh copy is on its way.

use super::adapt::{self, Extras};
use super::glass::GlassWrite;
use super::view::{Load, MissionPreview};
use super::{Effect, Guard, Ui};
use crate::feed::{self, Command, TerminalUpdate, Window};
use crate::model::{self, Collection, Model};
use anyhow::Result;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use st3_client::{
    Client, ClientError, Fence, LaunchReviseParameters, MessageSendParameters, Resource, TargetParameters,
    TimelineBody,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

/// A message sent from here that st has not reported back yet.
struct Pending {
    token: String,
    agent: String,
    text: String,
    at: String,
    message_id: Option<String>,
    failed: Option<String>,
    /// st did not answer, so the message may have arrived.
    unconfirmed: bool,
    /// What was asked of st, to ask again.
    effect: Effect,
    /// The exact request last sent; a retry repeats it so st can answer with the first result.
    sent: Arc<Mutex<Option<Sent>>>,
}

/// A message request as sent: st keys its receipt on the whole request.
#[derive(Clone, Debug)]
struct Sent {
    id: String,
    idempotency_key: String,
    snapshot_id: String,
}

/// Read evidence belongs to a presented frame, so navigation cannot cancel retries.
#[derive(Default)]
struct ReadReceipts {
    confirmed: HashSet<String>,
    pending: BTreeMap<String, Option<Instant>>,
}

impl ReadReceipts {
    fn displayed(&mut self, id: String, now: Instant) {
        if !self.confirmed.contains(&id) {
            self.pending.entry(id).or_insert(Some(now));
        }
    }
    fn next(&mut self, now: Instant) -> Option<String> {
        // Each read changes the snapshot fence. Publish serially to avoid racing receipts.
        if self.pending.values().any(Option::is_none) {
            return None;
        }
        let id = self
            .pending
            .iter()
            .find_map(|(id, retry)| retry.filter(|at| *at <= now).map(|_| id.clone()))?;
        self.pending.insert(id.clone(), None);
        Some(id)
    }
    fn completed(&mut self, id: String, succeeded: bool, now: Instant) {
        if succeeded {
            self.pending.remove(&id);
            if self.confirmed.len() >= 2048 {
                self.confirmed.clear();
            }
            self.confirmed.insert(id);
        } else {
            self.pending.insert(id, Some(now + Duration::from_secs(2)));
        }
    }
}

pub struct Context {
    pub client: Client,
    pub runtime: tokio::runtime::Runtime,
    pub incoming: mpsc::Receiver<feed::Update>,
    pub commands: tokio::sync::mpsc::UnboundedSender<Command>,
    pub person: String,
    pub cache_path: Option<std::path::PathBuf>,
    pub cached: Option<Model>,
    /// `stui --glasses` / `--glass NAME`: open glasses instead of the sidebar layout, at the
    /// named glass or the last one used on this device.
    pub glass: Option<Option<String>>,
}

/// The terminal the feed follows for the open terminal view.
struct Following {
    terminal_id: String,
    attachment_id: String,
    incarnation: String,
}

/// How often usage on screen is read again.
const USAGE_EVERY: Duration = Duration::from_secs(60);

/// Why usage could not be read, saying so plainly when the daemon predates the read.
fn usage_error(error: &st3_client::ClientError) -> String {
    match error {
        // A daemon from before the read answers its path with a bare 404.
        st3_client::ClientError::Api(st3_client::ErrorCode::NotFound, _, _) => {
            "This st does not serve usage yet: its daemon needs an update.".into()
        }
        st3_client::ClientError::Protocol(message) if message.starts_with("HTTP 404") => {
            "This st does not serve usage yet: its daemon needs an update.".into()
        }
        error => format!("Could not read usage: {}", error.plain()),
    }
}

enum Fetched {
    Read(String, Result<(), String>),
    Preview(String, Load<MissionPreview>),
    /// The message behind an unread-message item: sender, title and text.
    Body(String, String, Option<String>, String),
    Notice(String),
    /// Harness sessions st did not start, found on this machine.
    Sessions(Collection),
    /// The Fleet tab's machines and paired devices.
    Machines(Collection),
    /// Token spend over a period of this many hours, or why st could not say.
    Usage(u64, Result<st3_client::UsagePeriod, String>),
    /// st's conversation search for the palette's query, or why st could not say.
    Said(String, Result<st3_client::ConversationSearch, String>),
    Devices(Collection),
    /// A send finished: the pending token and st's message id, or why it failed and whether st's
    /// answer is unknown.
    Sent(String, Result<Option<String>, (String, bool)>),
    /// st started an agent asked for here.
    AgentStarted(String),
    /// st started a shell asked for here.
    TerminalStarted(String),
    /// A direct stream to an agent's (or a shell's) PTY session.
    Native {
        agent: String,
        direct: Direct,
    },
    /// Attaching again after the stream dropped: the new stream, or why not.
    Reattached {
        agent: String,
        outcome: Result<Direct, String>,
    },
    /// st gave no direct stream; follow its view of the terminal instead.
    NativeFailed {
        agent: String,
        runtime_ids: Vec<String>,
        reason: String,
    },
    /// st answered a glass write: the glass, the write's key, and the revision it accepted.
    GlassSaved {
        id: String,
        key: String,
        outcome: Result<Option<String>, String>,
    },
}

/// Send one glass write to st; its answer comes back as `Fetched::GlassSaved`.
fn save_glass(
    runtime: &tokio::runtime::Runtime,
    client: &Client,
    tx: &std::sync::mpsc::Sender<Fetched>,
    write: GlassWrite,
) {
    let client = client.clone();
    let tx = tx.clone();
    runtime.spawn(async move {
        let outcome = match &write {
            GlassWrite::Put { id, body, base, key } => match serde_json::from_str(body) {
                Ok(body) => client
                    .put_glass(
                        id,
                        &st3_client::GlassPut {
                            body,
                            base_revision: base.clone(),
                        },
                        key,
                    )
                    .await
                    .map(|saved| Some(saved.value.header.revision))
                    .map_err(|error| error.plain()),
                Err(error) => Err(error.to_string()),
            },
            GlassWrite::Delete { id, base, key } => client
                .delete_glass(
                    id,
                    &st3_client::GlassDelete {
                        base_revision: base.clone(),
                    },
                    key,
                )
                .await
                .map(|_| None)
                .map_err(|error| error.plain()),
        };
        let _ = tx.send(Fetched::GlassSaved {
            id: write.id().to_owned(),
            key: write.key().to_owned(),
            outcome,
        });
    });
}

pub fn run(context: Context) -> Result<()> {
    let Context {
        mut client,
        runtime,
        incoming,
        commands,
        person,
        cache_path,
        cached,
        glass,
    } = context;
    let (fetched_tx, fetched) = mpsc::channel::<Fetched>();
    let mut model = cached.unwrap_or_default();
    // The attention window is already this person's; nothing else names the actor.
    model.actor = person.clone();
    let mut extras = Extras::default();
    // Each conversation st has sent, kept after it closes so reopening it shows its last entries.
    let mut timelines: BTreeMap<String, st3_conversation_ui::Timeline> = BTreeMap::new();
    let mut failed: BTreeMap<String, String> = BTreeMap::new();
    // The agent or session whose conversation the feed holds.
    let mut conversing: Vec<String> = Vec::new();
    let mut preview_requested: HashSet<String> = HashSet::new();
    let mut body_requested: HashSet<String> = HashSet::new();
    let mut read_receipts = ReadReceipts::default();
    // Messages sent from here, shown at once until st reports them back.
    let mut pending: Vec<Pending> = Vec::new();
    let mut ui = Ui::new(adapt::world(&model, &person, &extras));
    ui.load_prefs();
    ui.build = true;
    ui.live = true;
    ui.glasses = glass.map(|name| {
        super::glass::Glasses::open(name, super::glass_store::path(&person))
    });
    // A glass opens where this device left it.
    ui.show_focused();
    if ui.glasses.is_none() {
        ui.flash(
            "The classic layout is going away: plain stui opens spaces, with Ctrl+S for this list",
        );
    }

    let _guard = Guard::enter(ui.glasses.is_some())?;
    // How images are drawn: asked of a terminal known to draw them, once, inside the
    // alternate screen and before any event is read. A terminal that never answers would
    // leave the query reading stdin and swallow keys, so others get half blocks unasked.
    ui.picker = Some(if super::attach::graphics_terminal() {
        ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks())
    } else {
        ratatui_image::picker::Picker::halfblocks()
    });
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let started = Instant::now();
    let mut changed = true;
    let stopping = super::stop_flag()?;
    let mut attached: Option<Following> = None;
    // The runtimes of the agent whose terminal view is open, to follow it again after a pause.
    let mut terminal_runtimes: Option<Vec<String>> = None;
    // Attaching a dropped terminal again: whether a try is out, and how many failed.
    let mut reattaching = false;
    let mut reattach_tries = 0_u32;
    // The tab shown on the last pass: opening a tab loads what only it needs.
    let mut shown_tab = usize::MAX;
    // When usage was last asked for and over how many hours, and whether that read is out.
    let mut usage_read: Option<(Instant, u64)> = None;
    let mut usage_reading = false;
    // The palette's conversation search: what st was last asked, and what is typed since when.
    let mut said_asked: Option<String> = None;
    let mut said_typed: Option<(String, Instant)> = None;
    let mut last_cache_save = Instant::now();
    // A closed terminal ends the loop: without this check a detached stui spins and keeps
    // polling the daemon forever.
    while !ui.quit
        && !stopping.load(std::sync::atomic::Ordering::Relaxed)
        && !crate::stdin_hung_up()
    {
        ui.tick = (started.elapsed().as_millis() / 100) as u64;
        if ui
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4))
        {
            ui.flash = None;
        }
        while let Ok(update) = incoming.try_recv() {
            match update {
                feed::Update::Connected(member) => {
                    client = member;
                    extras.live = false;
                    attached = None;
                    shown_tab = usize::MAX;
                    preview_requested.clear();
                    body_requested.clear();
                    changed = true;
                }
                feed::Update::Window {
                    window: Window::Glasses,
                    items,
                    ..
                } => {
                    ui.glasses_from_graph(
                        items
                            .into_iter()
                            .filter_map(|item| match item {
                                Resource::Glass(glass) => Some(glass),
                                _ => None,
                            })
                            .collect(),
                    );
                    changed = true;
                }
                feed::Update::Window {
                    window,
                    snapshot,
                    items,
                    has_more,
                } => {
                    // Back in touch: glass changes st has not confirmed go again, same keys.
                    if !extras.live {
                        for write in ui.unsent_glass_writes() {
                            save_glass(&runtime, &client, &fetched_tx, write);
                        }
                    }
                    let collection = Collection {
                        items,
                        snapshot: Some(snapshot),
                        truncated: has_more,
                        sync: None,
                    };
                    match window {
                        Window::Attention => model.now = collection,
                        Window::Missions => model.missions = collection,
                        Window::Agents => model.agents = collection,
                        Window::Glasses => {}
                    }
                    extras.live = true;
                    extras.offline = None;
                    model.last_connected =
                        Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                    changed = true;
                }
                feed::Update::Conversation {
                    target,
                    replace,
                    has_more,
                    items,
                } => {
                    failed.remove(&target);
                    ui.conversation_updated(&target);
                    timelines
                        .entry(target)
                        .or_default()
                        .apply(st3_conversation_ui::Frame {
                            replace,
                            has_more,
                            items,
                        });
                    // A message sent from here is done once st shows it in the conversation.
                    pending.retain(|pending| {
                        pending.message_id.as_ref().is_none_or(|id| {
                            !timelines.values().flat_map(|timeline| &timeline.items).any(|entry| {
                                matches!(&entry.body, TimelineBody::Message(message) if &message.message_id == id)
                            })
                        })
                    });
                    changed = true;
                }
                feed::Update::ConversationFailed {
                    target,
                    message,
                    permanent,
                } => {
                    // A permanent refusal is said once, without "retrying".
                    if !permanent {
                        ui.conversation_failed(&target, &message);
                    }
                    failed.insert(target, message);
                    changed = true;
                }
                feed::Update::WindowFailed(window, error) => {
                    ui.flash(format!("Could not load {window:?}: {error}"));
                }
                feed::Update::Offline(error) => {
                    extras.live = false;
                    extras.offline = Some(error);
                    attached = None;
                    save_cache(cache_path.as_deref(), &person, &model);
                    changed = true;
                }
                feed::Update::Terminal(update) => match update {
                    TerminalUpdate::Attached {
                        terminal_id,
                        attachment_id,
                        incarnation,
                    } => {
                        attached = Some(Following {
                            terminal_id,
                            attachment_id,
                            incarnation,
                        });
                    }
                    TerminalUpdate::Screen(screen) => {
                        if let Some(view) = ui.terminal.as_mut() {
                            view.lines = screen_lines(&screen);
                            view.cursor = screen
                                .cursor
                                .visible
                                .then_some((screen.cursor.row, screen.cursor.column));
                            view.stale = None;
                            if !screen.title.is_empty() {
                                view.title = format!("{} · {}", view.name, screen.title);
                            }
                        }
                    }
                    TerminalUpdate::Reconnecting(reason) => {
                        if let Some(view) = ui.terminal.as_mut() {
                            view.stale = Some(reason);
                        }
                    }
                    TerminalUpdate::Ended { restarted, reason } => {
                        attached = None;
                        match ui.terminal.as_mut() {
                            Some(view) => {
                                view.ended = Some(if restarted {
                                    "Terminal restarted; open it again to follow the new one".into()
                                } else {
                                    reason
                                })
                            }
                            None if !restarted => {
                                ui.flash(format!("Could not open the terminal: {reason}"))
                            }
                            None => {}
                        }
                    }
                },
            }
        }
        while let Ok(result) = fetched.try_recv() {
            match result {
                Fetched::Read(id, result) => {
                    read_receipts.completed(id, result.is_ok(), Instant::now());
                }
                Fetched::Sent(token, outcome) => {
                    if let Some(entry) = pending.iter_mut().find(|entry| entry.token == token) {
                        match outcome {
                            Ok(id) => {
                                entry.message_id = id;
                                entry.failed = None;
                            }
                            Err((error, unconfirmed)) => {
                                entry.failed = Some(error);
                                entry.unconfirmed = unconfirmed;
                            }
                        }
                    }
                }
                Fetched::Preview(id, preview) => {
                    extras.previews.insert(id, preview);
                }
                Fetched::Body(id, from, title, content) => {
                    extras.bodies.insert(id, (from, title, content));
                }
                Fetched::Notice(notice) => ui.flash(notice),
                Fetched::Sessions(native) => {
                    model.sessions = native;
                }
                Fetched::Machines(machines) => model.machines = machines,
                Fetched::Said(query, outcome) => {
                    ui.said = Some((query, outcome));
                    changed = true;
                }
                Fetched::Usage(hours, outcome) => {
                    usage_reading = false;
                    if hours == ui.usage_hours {
                        match outcome {
                            Ok(period) => model.usage = Some(Ok(period)),
                            // Rows already shown stay; a passing failure is only mentioned.
                            Err(why) if matches!(model.usage, Some(Ok(_))) => {
                                ui.flash(format!("Could not read usage again: {why}"))
                            }
                            Err(why) => model.usage = Some(Err(why)),
                        }
                    }
                }
                Fetched::Devices(devices) => model.devices = devices,
                Fetched::GlassSaved { id, key, outcome } => ui.glass_saved(&id, &key, outcome),
                Fetched::AgentStarted(id) => ui.agent_started(id),
                Fetched::TerminalStarted(id) => ui.terminal_started(id),
                Fetched::Native { agent, direct } => {
                    let (rows, columns) = ui.terminal_size.get();
                    if let Some(view) = ui
                        .terminal
                        .as_mut()
                        .filter(|view| view.agent == agent && view.native.is_none())
                    {
                        view.native = Some(super::pty::NativeTerminal::spawn(
                            direct.stream,
                            &direct.name,
                            direct.incarnation,
                            rows,
                            columns,
                        ));
                        view.stale = None;
                    }
                }
                Fetched::Reattached { agent, outcome } => {
                    reattaching = false;
                    if let Some(native) = ui
                        .terminal
                        .as_ref()
                        .filter(|view| view.agent == agent)
                        .and_then(|view| view.native.as_ref())
                    {
                        match outcome {
                            Ok(direct) => native.reconnect(direct.stream, &direct.name),
                            Err(reason) if reason.contains("restarted") => native.give_up(reason),
                            // Still unreachable: the next try waits longer.
                            Err(_) => reattach_tries += 1,
                        }
                    }
                }
                Fetched::NativeFailed {
                    agent,
                    runtime_ids,
                    reason,
                } => {
                    if agent.starts_with("terminal/") {
                        // A shell has no other view to fall back to.
                        if let Some(view) = ui.terminal.as_mut().filter(|view| view.agent == agent)
                        {
                            view.ended = Some(format!("could not attach: {reason}"));
                        }
                    } else if ui.terminal.as_ref().is_some_and(|view| view.agent == agent) {
                        ui.flash(format!(
                            "No direct terminal ({reason}); showing st's view of it"
                        ));
                        let _ = commands.send(Command::Follow { runtime_ids });
                    }
                }
            }
            changed = true;
        }

        // What the selection needs: a conversation, or a launch preview.
        let (tab, selected) = ui.focus();
        // What a tab needs when it opens: harness sessions st did not start (Agents), and the
        // machines and devices (Fleet). They are read then, never on a timer.
        if tab != shown_tab {
            // The terminal is followed only while it is on screen: leaving the Agents tab
            // pauses it, and coming back shows the current screen first.
            if let (Some(view), Some(runtime_ids)) = (ui.terminal.as_mut(), &terminal_runtimes)
                && view.ended.is_none()
            {
                if shown_tab == 1 {
                    let _ = commands.send(Command::Unfollow);
                    attached = None;
                    view.stale = Some("paused while hidden".into());
                } else if tab == 1 {
                    let _ = commands.send(Command::Follow {
                        runtime_ids: runtime_ids.clone(),
                    });
                    view.stale = Some("reconnecting".into());
                }
            }
            shown_tab = tab;
            if tab == 1 || model.sessions.snapshot.is_none() {
                let client = client.clone();
                let tx = fetched_tx.clone();
                runtime.spawn(async move {
                    match model::read_native_sessions(&client).await {
                        Ok(native) => {
                            let _ = tx.send(Fetched::Sessions(native));
                        }
                        Err(error) => {
                            let _ = tx.send(Fetched::Notice(format!(
                                "Could not look for other harness sessions: {error}"
                            )));
                        }
                    }
                });
            }
            // Glasses show the fleet in the status line and the palette, so they need the
            // machines from the start rather than when a Fleet tab opens.
            if tab == 3 || (ui.glasses.is_some() && model.machines.snapshot.is_none()) {
                let client = client.clone();
                let tx = fetched_tx.clone();
                runtime.spawn(async move {
                    let (machines, devices) =
                        tokio::join!(model::read_machines(&client), model::read_devices(&client));
                    for (result, what) in [(machines, "machines"), (devices, "devices")] {
                        let _ = tx.send(match result {
                            Ok(collection) if what == "machines" => Fetched::Machines(collection),
                            Ok(collection) => Fetched::Devices(collection),
                            Err(error) => {
                                Fetched::Notice(format!("Could not load {what}: {error}"))
                            }
                        });
                    }
                });
            }
        }
        // Ctrl+K asks st's conversation search once what is typed has been still for a moment;
        // an answer to an earlier query is dropped where it lands (Ui::said_choices).
        match ui.said_wanted() {
            Some(query) if said_asked.as_deref() != Some(query.as_str()) => match &said_typed {
                Some((typed, at)) if *typed == query => {
                    if at.elapsed() >= Duration::from_millis(250) && extras.live {
                        said_asked = Some(query.clone());
                        let client = client.clone();
                        let tx = fetched_tx.clone();
                        runtime.spawn(async move {
                            let outcome = client
                                .conversation_search(&query, None, None, None, Some(20))
                                .await
                                .map(|envelope| envelope.value)
                                .map_err(|error| error.plain());
                            let _ = tx.send(Fetched::Said(query, outcome));
                        });
                    }
                }
                _ => said_typed = Some((query, Instant::now())),
            },
            None => {
                said_asked = None;
                said_typed = None;
            }
            _ => {}
        }
        // Usage has no stream: it is read while something shows it, again each minute, and at
        // once over a new period.
        if let Some(hours) = ui.usage_wanted() {
            let new_period = usage_read.is_some_and(|(_, read)| read != hours);
            if new_period {
                model.usage = None;
                changed = true;
            }
            let due = usage_read.is_none_or(|(at, _)| at.elapsed() >= USAGE_EVERY) || new_period;
            if due && !usage_reading {
                usage_read = Some((Instant::now(), hours));
                usage_reading = true;
                let client = client.clone();
                let tx = fetched_tx.clone();
                runtime.spawn(async move {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |since| since.as_millis() as u64);
                    let since = now.saturating_sub(hours * 3_600_000);
                    let outcome = client
                        .usage_period(Some(since), None)
                        .await
                        .map(|envelope| envelope.value)
                        .map_err(|error| usage_error(&error));
                    let _ = tx.send(Fetched::Usage(hours, outcome));
                });
            }
        }
        // Every conversation on screen rides the feed's socket (the focused one first): st
        // pushes each change, so nothing here reads one again on a timer.
        let wanted = ui.live_conversations();
        if wanted != conversing {
            let _ = commands.send(Command::Converse {
                targets: wanted.clone(),
            });
            conversing = wanted;
            changed = true;
        }
        if tab == 0
            && let Some(id) = selected.clone()
            && !preview_requested.contains(&id)
            && let Some(item) = model
                .attention()
                .find(|item| item.header.id == id && item.attention_kind == "launch-approval")
        {
            preview_requested.insert(id.clone());
            let client = client.clone();
            let tx = fetched_tx.clone();
            let source = item.source_id.clone();
            let name = item
                .mission_id
                .clone()
                .unwrap_or_else(|| item.title.clone())
                .trim_start_matches("mission/")
                .to_owned();
            runtime.spawn(async move {
                let preview = match client.launch_variants_list(&source, None, Some(50)).await {
                    Err(error) => Load::Failed(format!("Could not load the proposed mission: {error}")),
                    Ok(variants) => {
                        let latest = variants
                            .value
                            .items
                            .iter()
                            .filter_map(|item| match item {
                                Resource::LaunchVariant(variant) => Some(variant),
                                _ => None,
                            })
                            .max_by_key(|variant| variant.ordinal);
                        match latest {
                            None => Load::Failed("The planner has not proposed a mission yet.".into()),
                            Some(variant)
                                if variant.normalized_mission.as_object().is_none_or(|fields| fields.is_empty()) =>
                            {
                                let mut reason = format!(
                                    "The planner's latest candidate ({}) has no preview, so there is no mission to show yet.",
                                    variant.status
                                );
                                for diagnostic in variant.diagnostics.iter().take(3) {
                                    if let Some(message) = diagnostic.get("message").and_then(|value| value.as_str()) {
                                        reason.push_str(&format!("\n• {message}"));
                                    }
                                }
                                Load::Failed(reason)
                            }
                            Some(variant) => Load::Ready(adapt::preview(&name, &variant.normalized_mission)),
                        }
                    }
                };
                let _ = tx.send(Fetched::Preview(id, preview));
            });
        }

        // An unread-message item names its message; load the message itself.
        if tab == 0
            && let Some(id) = selected.clone()
            && !body_requested.contains(&id)
            && let Some(item) = model
                .attention()
                .find(|item| item.header.id == id && item.attention_kind == "unread-message")
        {
            body_requested.insert(id.clone());
            let client = client.clone();
            let tx = fetched_tx.clone();
            let source = item.source_id.clone();
            runtime.spawn(async move {
                if let Ok(found) = client.messages_get(&source).await
                    && let Resource::Message(message) = found.value
                {
                    let _ = tx.send(Fetched::Body(
                        id,
                        message.from,
                        message.title,
                        message.content,
                    ));
                }
            });
        }
        let mut effects = Vec::new();
        for effect in std::mem::take(&mut ui.effects) {
            // A glass change is kept until st confirms it, and goes once st is reachable.
            if let Effect::SaveGlass(write) = effect {
                if extras.live {
                    save_glass(&runtime, &client, &fetched_tx, write);
                }
                continue;
            }
            if !extras.live && !matches!(effect, Effect::CloseTerminal) {
                ui.flash("Offline · reconnect before acting; nothing was queued");
                continue;
            }
            match effect {
                Effect::OpenTerminal { agent } => {
                    // The PTY session's own bytes, through st's raw stream to whichever host owns
                    // it; st's screen view (the feed follows it on its socket) only when st cannot
                    // give a direct stream.
                    let found = model
                        .agents()
                        .find(|candidate| candidate.header.id == agent);
                    let runtime_ids = found
                        .map(|candidate| candidate.runtime_ids.clone())
                        .unwrap_or_default();
                    let name = found.map(crate::agent_label).unwrap_or_else(|| {
                        if agent.starts_with("terminal/") {
                            "shell".into()
                        } else {
                            agent.clone()
                        }
                    });
                    attached = None;
                    terminal_runtimes = Some(runtime_ids.clone());
                    let _ = commands.send(Command::Unfollow);
                    {
                        let client = client.clone();
                        let tx = fetched_tx.clone();
                        let agent = agent.clone();
                        runtime.spawn(async move {
                            let attached = attach_direct(&client, &agent, &runtime_ids, None).await;
                            let _ = tx.send(match attached {
                                Ok(direct) => Fetched::Native { agent, direct },
                                Err(reason) => Fetched::NativeFailed {
                                    agent,
                                    runtime_ids,
                                    reason,
                                },
                            });
                        });
                    }
                    {
                        ui.terminal = Some(super::TerminalView {
                            agent: agent.clone(),
                            title: name.clone(),
                            name,
                            lines: Vec::new(),
                            cursor: None,
                            stale: Some("connecting".into()),
                            ended: None,
                            native: None,
                        });
                    }
                }
                Effect::CloseTerminal => {
                    // Leaving the terminal view stops following it; the feed ends the viewer.
                    let _ = commands.send(Command::Unfollow);
                    attached = None;
                    terminal_runtimes = None;
                    ui.terminal = None;
                }
                Effect::TerminalKey(key) => {
                    if let Some(current) = &attached {
                        let client = client.clone();
                        let tx = fetched_tx.clone();
                        let terminal = current.terminal_id.clone();
                        let incarnation = current.incarnation.clone();
                        runtime.spawn(async move {
                            if let Err(error) =
                                crate::send_terminal_key(&client, &terminal, &incarnation, key)
                                    .await
                            {
                                let _ = tx.send(Fetched::Notice(format!(
                                    "Key not sent: {}",
                                    plain(&error)
                                )));
                            }
                        });
                    } else {
                        ui.flash("The terminal is not connected; that key was not sent");
                    }
                }
                other => effects.push(other),
            }
        }
        for effect in effects {
            let (effect, token, sent) = match effect {
                Effect::Resend { entry } => {
                    let token = entry.trim_start_matches("pending:");
                    let Some(retry) = pending.iter_mut().find(|pending| pending.token == token)
                    else {
                        continue;
                    };
                    retry.failed = None;
                    retry.unconfirmed = false;
                    changed = true;
                    (
                        retry.effect.clone(),
                        Some(retry.token.clone()),
                        Some(retry.sent.clone()),
                    )
                }
                Effect::Forget { entry } => {
                    let token = entry.trim_start_matches("pending:");
                    pending.retain(|pending| pending.token != token);
                    changed = true;
                    continue;
                }
                Effect::Send {
                    ref agent,
                    ref text,
                    ..
                }
                | Effect::Discuss {
                    to: ref agent,
                    ref text,
                    ..
                } => {
                    let token = uuid::Uuid::now_v7().to_string();
                    let sent = Arc::new(Mutex::new(None));
                    pending.push(Pending {
                        token: token.clone(),
                        agent: agent.clone(),
                        text: text.clone(),
                        at: chrono::Local::now().format("%H:%M").to_string(),
                        message_id: None,
                        failed: None,
                        unconfirmed: false,
                        effect: effect.clone(),
                        sent: sent.clone(),
                    });
                    changed = true;
                    (effect, Some(token), Some(sent))
                }
                other => (other, None, None),
            };
            let started = matches!(effect, Effect::CreateAgent { .. });
            let shell = matches!(effect, Effect::CreateTerminal { .. });
            let client = client.clone();
            let tx = fetched_tx.clone();
            let person = person.clone();
            let model = model.clone();
            runtime.spawn(async move {
                let outcome =
                    perform_steadily(&client, &person, &model, effect, sent.as_deref()).await;
                if let Some(token) = token {
                    let _ = tx.send(Fetched::Sent(
                        token,
                        outcome.as_ref().map(|(_, id)| id.clone()).map_err(|error| {
                            // A transport failure left st's answer unknown; anything else is
                            // st saying no, or never reaching it.
                            let unconfirmed = error
                                .downcast_ref::<ClientError>()
                                .is_some_and(|error| matches!(error, ClientError::Transport(_)));
                            (plain(error), unconfirmed)
                        }),
                    ));
                }
                if started && let Ok((_, Some(agent))) = &outcome {
                    let _ = tx.send(Fetched::AgentStarted(agent.clone()));
                }
                if shell && let Ok((_, Some(terminal))) = &outcome {
                    let _ = tx.send(Fetched::TerminalStarted(terminal.clone()));
                }
                let _ = tx.send(Fetched::Notice(match outcome {
                    Ok((notice, _)) => notice,
                    Err(error) => format!("Not done: {}", plain(&error)),
                }));
            });
        }

        if changed {
            extras.conversations = conversations(&model, &person, &timelines, &failed, &conversing);
            for entry in &pending {
                if let Some(Load::Ready(entries)) = extras.conversations.get_mut(&entry.agent) {
                    entries.push(super::view::Entry {
                        id: format!("pending:{}", entry.token),
                        at: entry.at.clone(),
                        body: super::view::Body::Pending {
                            text: entry.text.clone(),
                            failed: entry.failed.clone(),
                            unconfirmed: entry.unconfirmed,
                        },
                    });
                }
            }
            ui.set_world(adapt::world(&model, &person, &extras));
            changed = false;
            if last_cache_save.elapsed() >= Duration::from_secs(60) {
                save_cache(cache_path.as_deref(), &person, &model);
                last_cache_save = Instant::now();
            }
        }
        ui.step_voice();
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        terminal.draw(|frame| ui.render(frame))?;
        execute!(io::stdout(), EndSynchronizedUpdate)?;
        let visible = ui.visible_messages();
        let incoming: HashSet<_> = timelines
            .values()
            .flat_map(|timeline| &timeline.items)
            .filter_map(|entry| match &entry.body {
                TimelineBody::Message(message)
                    if message.to.as_deref() == Some(person.as_str()) =>
                {
                    Some(message.message_id.as_str())
                }
                _ => None,
            })
            .collect();
        for id in visible
            .into_iter()
            .filter(|id| incoming.contains(id.as_str()))
        {
            read_receipts.displayed(id, Instant::now());
        }
        if extras.live
            && let Some(id) = read_receipts.next(Instant::now())
        {
            let client = client.clone();
            let person = person.clone();
            let tx = fetched_tx.clone();
            runtime.spawn(async move {
                let result = acknowledge_visible_message(&client, &person, &id)
                    .await
                    .map_err(|error| error.to_string());
                let _ = tx.send(Fetched::Read(id, result));
            });
        }
        // A stream that dropped while the program ran attaches again, to the same incarnation,
        // after a wait that grows with each try.
        if !reattaching
            && extras.live
            && let Some(view) = ui.terminal.as_ref()
            && let Some(native) = view.native.as_ref()
            && let Some(at) = native.dropped()
            && at.elapsed() >= Duration::from_secs(2_u64.pow(reattach_tries.min(5)))
        {
            reattaching = true;
            let client = client.clone();
            let tx = fetched_tx.clone();
            let agent = view.agent.clone();
            let expected = native.incarnation.clone();
            let runtime_ids = terminal_runtimes.clone().unwrap_or_default();
            runtime.spawn(async move {
                let outcome = attach_direct(&client, &agent, &runtime_ids, Some(&expected)).await;
                let _ = tx.send(Fetched::Reattached { agent, outcome });
            });
        }
        if ui
            .terminal
            .as_ref()
            .and_then(|view| view.native.as_ref())
            .is_none()
        {
            reattach_tries = 0;
        }
        // While an attached terminal's output flows, or voice listens, draw it as it comes.
        let flowing = ui.voice.is_some()
            || ui
                .terminal
                .as_ref()
                .and_then(|view| view.native.as_ref())
                .is_some_and(|native| native.flowing());
        if event::poll(Duration::from_millis(if flowing { 16 } else { 80 }))? {
            // crossterm's read never returns on a closed terminal, so check for one before each.
            while !stopping.load(std::sync::atomic::Ordering::Relaxed) && !crate::stdin_hung_up() {
                match event::read()? {
                    Event::Key(key)
                        if !extras.live
                            && key.code == crossterm::event::KeyCode::Char('r')
                            && !ui.editing =>
                    {
                        let _ = commands.send(Command::Reconnect);
                    }
                    Event::Key(key) => ui.key(key),
                    Event::Paste(text) => ui.paste(text),
                    Event::Mouse(mouse) => ui.mouse(mouse),
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
    // Leave no attachment behind.
    if let Some(current) = attached.take() {
        let _ = runtime.block_on(feed::detach(
            &client,
            &current.terminal_id,
            &current.attachment_id,
            &current.incarnation,
        ));
    }
    save_cache(cache_path.as_deref(), &person, &model);
    Ok(())
}

fn save_cache(path: Option<&std::path::Path>, person: &str, model: &Model) {
    if let Some(path) = path
        && model.missions.snapshot.is_some()
    {
        let _ = crate::cache::save(path, person, model);
    }
}

/// A terminal screen as styled lines, the way the old screens drew it.
fn screen_lines(screen: &st3_client::TerminalScreen) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line, Span};
    screen
        .lines
        .iter()
        .map(|screen_line| {
            let mut line = if screen_line.redacted {
                Line::from("[redacted]")
            } else if screen_line.runs.is_empty() {
                Line::from(super::text::sanitize(&screen_line.text))
            } else {
                Line::from(
                    screen_line
                        .runs
                        .iter()
                        .map(|run| {
                            Span::styled(
                                super::text::sanitize(&run.text),
                                crate::terminal_run_style(run),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            };
            // st cut a line longer than it sends; say so rather than let it look complete.
            if screen_line.truncated {
                line.spans.push(Span::styled("…", super::theme::dim()));
            }
            line
        })
        .collect()
}

/// Each conversation to draw: what st sent, and why it could not send more.
fn conversations(
    model: &Model,
    person: &str,
    timelines: &BTreeMap<String, st3_conversation_ui::Timeline>,
    failed: &BTreeMap<String, String>,
    conversing: &[String],
) -> BTreeMap<String, Load<Vec<super::view::Entry>>> {
    let mut out = BTreeMap::new();
    let names = adapt::names(model, person);
    let targets = timelines
        .keys()
        .chain(failed.keys())
        .chain(conversing)
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for target in targets {
        let load = match (timelines.get(target), failed.get(target)) {
            // Half a conversation is worse than none: say why instead.
            (Some(timeline), _)
                if let Some(reason) = adapt::unreadable_transcript(&timeline.items) =>
            {
                Load::Failed(reason)
            }
            // A failure after the conversation loaded is said on the rule above its message
            // box, where it clears once st catches up; the feed retries on its own.
            (Some(timeline), _) => Load::Ready(adapt::conversation(&timeline.items, &names)),
            (None, Some(error)) => {
                Load::Failed(format!("Could not load this conversation: {error}"))
            }
            (None, None) => Load::Loading,
        };
        out.insert(target.to_owned(), load);
    }
    out
}

/// The draw loop supplies body-visible IDs. Metadata fetches, caches and hidden pages never
/// call this. Failed publication remains queued after navigation until confirmed.
async fn acknowledge_visible_message(client: &Client, person: &str, id: &str) -> Result<()> {
    let found = client.messages_get(id).await?;
    let Resource::Message(message) = found.value else {
        anyhow::bail!("message disappeared");
    };
    if message.to != person || matches!(message.state.as_str(), "read" | "closed") {
        return Ok(());
    }
    let mut fence = Fence {
        snapshot_id: found.snapshot.id,
        ..Fence::default()
    };
    fence
        .subject_revisions
        .insert(message.header.id.clone(), message.header.revision);
    let (action, idem) = crate::action_pair();
    let result = client
        .message_read(
            action,
            idem,
            fence,
            TargetParameters {
                target_id: message.header.id,
                ..Default::default()
            },
        )
        .await?;
    anyhow::ensure!(
        matches!(result.value.status, st3_client::ActionStatus::Completed),
        "read receipt was not completed"
    );
    Ok(())
}

/// `perform`, tried again while st refused it only because it raced a busy store (or asked to
/// slow down, or was not answering at all): each try reads a fresh fence, and nothing was
/// applied, so a new request is safe. A timeout, which may have been applied, is not retried
/// here; an unconfirmed send keeps its exact request for that.
async fn perform_steadily(
    client: &Client,
    person: &str,
    model: &Model,
    effect: Effect,
    sent: Option<&Mutex<Option<Sent>>>,
) -> Result<(String, Option<String>)> {
    let mut wait = Duration::from_millis(50);
    for _ in 0..7 {
        match perform(client, person, model, effect.clone(), sent).await {
            Err(error) if not_applied(&error) => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(2));
            }
            outcome => return outcome,
        }
    }
    perform(client, person, model, effect, sent).await
}

/// Whether st refused a request in a way that guarantees it applied nothing and a fresh try
/// may succeed.
fn not_applied(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<ClientError>())
        .any(|error| {
            matches!(
                error,
                ClientError::Api(
                    st3_client::ErrorCode::StaleFence | st3_client::ErrorCode::RateLimited,
                    ..
                ) | ClientError::Unreachable(_)
            )
        })
}

/// An error as a person reads it: st's errors in plain words, anything else as it is.
pub(crate) fn plain(error: &anyhow::Error) -> String {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ClientError>())
        .map(ClientError::plain)
        .unwrap_or_else(|| error.to_string())
}

async fn perform(
    client: &Client,
    person: &str,
    model: &Model,
    effect: Effect,
    sent: Option<&Mutex<Option<Sent>>>,
) -> Result<(String, Option<String>)> {
    match effect {
        // Glass writes and retries never reach here: the loop handles them itself.
        Effect::SaveGlass(_) | Effect::Resend { .. } | Effect::Forget { .. } => {
            Ok((String::new(), None))
        }
        Effect::StopAgent { agent } => {
            let runtime = model
                .agents()
                .find(|candidate| candidate.header.id == agent)
                .and_then(|candidate| candidate.runtime_ids.first().cloned())
                .ok_or_else(|| anyhow::anyhow!("that agent is not running"))?;
            let Resource::Runtime(runtime) = client.runtimes_get(&runtime).await?.value else {
                anyhow::bail!("that agent's runtime is gone");
            };
            let terminal = runtime
                .terminal_id
                .ok_or_else(|| anyhow::anyhow!("that agent has no terminal to stop it through"))?;
            let incarnation = client
                .terminal_screen(&terminal)
                .await?
                .value
                .runtime_incarnation;
            crate::send_terminal_key(
                client,
                &terminal,
                &incarnation,
                crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Esc),
            )
            .await?;
            Ok(("Stopped its turn".into(), None))
        }
        Effect::CreateTerminal { name } => {
            let snapshot = client.capabilities().await?.snapshot.id;
            let (id, idem) = crate::action_pair();
            let result = client
                .terminal_create(
                    id,
                    idem,
                    Fence {
                        snapshot_id: snapshot,
                        ..Fence::default()
                    },
                    st3_client::TerminalCreateParameters {
                        name: name.clone(),
                        host: None,
                        cwd: None,
                    },
                )
                .await?;
            let terminal = result
                .value
                .affected_ids
                .into_iter()
                .find(|id| id.starts_with("terminal/"));
            Ok((format!("Started shell {name}"), terminal))
        }
        Effect::CreateAgent {
            name,
            harness,
            model,
            effort,
            host,
            message,
        } => {
            let snapshot = client.capabilities().await?.snapshot.id;
            let (id, idem) = crate::action_pair();
            let result = client
                .agent_create(
                    id,
                    idem,
                    Fence {
                        snapshot_id: snapshot,
                        ..Fence::default()
                    },
                    st3_client::AgentCreateParameters {
                        name: name.clone(),
                        harness,
                        host,
                        model,
                        effort,
                        workspace: None,
                        description: None,
                        message,
                    },
                )
                .await?;
            // The new agent's id, so its conversation opens in place of the form.
            let agent = result
                .value
                .affected_ids
                .into_iter()
                .find(|id| id.starts_with("agent/"));
            Ok((format!("Starting {name}"), agent))
        }
        Effect::Attention {
            id,
            action,
            reason,
            answer,
        } => {
            crate::attention_action(client, person, &id, &action, reason, answer)
                .await
                .map(|notice| (notice, None))
        }
        Effect::LaunchRevise { id, feedback } => {
            let current = client.attention_get(&id).await?;
            let Resource::Attention(attention) = &current.value else {
                anyhow::bail!("This launch changed; look again");
            };
            let launch = client.launches_get(&attention.source_id).await?;
            let Resource::Launch(resource) = launch.value else {
                anyhow::bail!("The launch is gone");
            };
            let mut fence = Fence {
                snapshot_id: launch.snapshot.id,
                ..Fence::default()
            };
            fence
                .subject_revisions
                .insert(resource.header.id.clone(), resource.header.revision);
            let (action_id, idem) = crate::action_pair();
            client
                .launch_revise(
                    action_id,
                    idem,
                    fence,
                    LaunchReviseParameters {
                        launch_id: resource.header.id,
                        feedback,
                    },
                )
                .await?;
            Ok(("Sent your changes to the planner".into(), None))
        }
        Effect::Reply { id, to, text } => {
            let current = client.attention_get(&id).await?;
            let Resource::Attention(attention) = &current.value else {
                anyhow::bail!("This message changed; look again");
            };
            send_message(
                client,
                &to,
                text,
                None,
                Some(attention.source_id.clone()),
                None,
                Vec::new(),
                None,
            )
            .await?;
            Ok(("Reply sent".into(), None))
        }
        Effect::Discuss { to, title, text } => {
            let (to, session) = (
                to.clone(),
                model
                    .agents()
                    .find(|candidate| candidate.header.id == to)
                    .and_then(|agent| agent.current_session_id.clone()),
            );
            let id = send_message(
                client,
                &to,
                text,
                Some(title),
                None,
                session,
                Vec::new(),
                sent,
            )
            .await?;
            Ok((
                "Sent; the reply will show here and in their conversation".into(),
                id,
            ))
        }
        Effect::CancelRun { mission } => {
            let found = model
                .missions()
                .find(|candidate| candidate.header.id == mission)
                .ok_or_else(|| anyhow::anyhow!("That mission is gone"))?;
            let run = found
                .runs
                .last()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("It has no run"))?;
            let generation = found.run_generations.get(&run).cloned();
            let snapshot = client.capabilities().await?.snapshot.id;
            let fence = Fence {
                snapshot_id: snapshot,
                mission_generation: generation,
                ..Fence::default()
            };
            let (id, idem) = crate::action_pair();
            client
                .mission_cancel(
                    id,
                    idem,
                    fence,
                    st3_client::TargetParameters {
                        target_id: run.clone(),
                        reason: Some(format!("cancelled from stui by {person}")),
                        ..Default::default()
                    },
                )
                .await?;
            Ok((format!("Cancelled {run}"), None))
        }
        Effect::CreateLaunch {
            title,
            request,
            mission,
            workspace,
        } => {
            let snapshot = client.capabilities().await?.snapshot.id;
            let (id, idem) = crate::action_pair();
            client
                .launch_create(
                    id,
                    idem,
                    Fence {
                        snapshot_id: snapshot,
                        ..Fence::default()
                    },
                    st3_client::LaunchCreateParameters {
                        title,
                        request,
                        target: st3_client::LaunchTarget::NewMission {
                            mission_id: mission,
                            workspace,
                        },
                        provider: None,
                        model: None,
                        effort: None,
                    },
                )
                .await?;
            Ok((
                "Launch created; the planner's proposal will appear on Home".into(),
                None,
            ))
        }
        Effect::RevokeDevice { id } => {
            let snapshot = client.capabilities().await?.snapshot.id;
            let (action, idem) = crate::action_pair();
            client
                .pairing_revoke(
                    action,
                    idem,
                    Fence {
                        snapshot_id: snapshot,
                        ..Fence::default()
                    },
                    st3_client::TargetParameters {
                        target_id: id,
                        ..Default::default()
                    },
                )
                .await?;
            Ok(("Device revoked".into(), None))
        }
        Effect::OpenTerminal { .. } | Effect::TerminalKey(_) | Effect::CloseTerminal => {
            Ok((String::new(), None))
        }
        Effect::Send { agent, text, tags } => {
            let session = model
                .agents()
                .find(|candidate| candidate.header.id == agent)
                .and_then(|agent| agent.current_session_id.clone());
            let id = send_message(client, &agent, text, None, None, session, tags, sent).await?;
            Ok(("Message sent".into(), id))
        }
    }
}

/// Send a message. `sent` keeps the exact request: when it already holds one, that request goes
/// again first, and st answers a repeat of a request it accepted with the first result, so a
/// message st took but never confirmed is not sent twice.
async fn send_message(
    client: &Client,
    to: &str,
    content: String,
    title: Option<String>,
    in_reply_to: Option<String>,
    session_id: Option<String>,
    tags: Vec<String>,
    sent: Option<&Mutex<Option<Sent>>>,
) -> Result<Option<String>> {
    let parameters = MessageSendParameters {
        to: to.to_owned(),
        content,
        title,
        in_reply_to,
        session_id,
        tags,
        signature: None,
    };
    let message_id = |result: st3_client::Envelope<st3_client::ActionResult>| {
        // The new message's id, so the pending copy can give way to the real one.
        result
            .value
            .affected_ids
            .into_iter()
            .find(|id| id.starts_with("message/"))
    };
    let earlier = sent.and_then(|sent| sent.lock().ok()?.clone());
    if let Some(earlier) = earlier {
        let fence = Fence {
            snapshot_id: earlier.snapshot_id,
            ..Fence::default()
        };
        match client
            .message_send(
                earlier.id,
                earlier.idempotency_key,
                fence,
                parameters.clone(),
            )
            .await
        {
            Ok(result) => return Ok(message_id(result)),
            // st checks a repeat before the fence: a stale fence means it never took it.
            Err(error) if error.to_string().contains("StaleFence") => {}
            Err(error) => return Err(error.into()),
        }
    }
    // A send only needs a current snapshot. Take a fresh one each time, and once more if the
    // graph moves between reading it and sending: a stale fence is not the person's problem.
    let mut last = None;
    for _ in 0..2 {
        let snapshot = client.capabilities().await?.snapshot.id;
        let (id, idem) = crate::action_pair();
        if let Some(sent) = sent
            && let Ok(mut slot) = sent.lock()
        {
            *slot = Some(Sent {
                id: id.clone(),
                idempotency_key: idem.clone(),
                snapshot_id: snapshot.clone(),
            });
        }
        let fence = Fence {
            snapshot_id: snapshot,
            ..Fence::default()
        };
        match client
            .message_send(id, idem, fence, parameters.clone())
            .await
        {
            Ok(result) => return Ok(message_id(result)),
            Err(error) if error.to_string().contains("StaleFence") => last = Some(error),
            Err(error) => return Err(error.into()),
        }
    }
    Err(last
        .map(Into::into)
        .unwrap_or_else(|| anyhow::anyhow!("the graph kept changing; try again")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_receipt_retries_survive_navigation_and_serialize_snapshot_changes() {
        let now = Instant::now();
        let mut receipts = ReadReceipts::default();
        receipts.displayed("message/one".into(), now);
        assert_eq!(receipts.next(now).as_deref(), Some("message/one"));
        receipts.displayed("message/two".into(), now);
        assert!(
            receipts.next(now).is_none(),
            "do not race the in-flight snapshot"
        );
        receipts.completed("message/one".into(), false, now);
        assert_eq!(receipts.next(now).as_deref(), Some("message/two"));
        receipts.completed("message/two".into(), true, now);
        assert!(receipts.next(now).is_none(), "retry backs off");
        assert_eq!(
            receipts.next(now + Duration::from_secs(2)).as_deref(),
            Some("message/one"),
            "retry without another displayed frame"
        );
        receipts.completed("message/one".into(), true, now);
        receipts.displayed("message/one".into(), now);
        assert!(
            receipts.next(now + Duration::from_secs(3)).is_none(),
            "redrawing does not resend a confirmed receipt"
        );
    }

    #[test]
    fn a_visible_message_header_without_its_body_is_not_a_read() {
        use super::super::view::{Body, Entry};
        let entries = vec![Entry {
            id: "message/reply".into(),
            at: "12:00".into(),
            body: Body::Mail {
                from: "keeper".into(),
                to: "you".into(),
                subject: "Reply".into(),
                body: "Here is the reply.".into(),
                delivered: false,
                dictated: false,
            },
        }];
        let cache = super::super::conversation::Cache::default();
        let doc = cache.render(
            &entries,
            90,
            &HashSet::new(),
            "*",
            st3_conversation_ui::Density::Full,
        );
        let body_start = doc.messages[0].1.start as u16;
        let ui = Ui::new(super::super::demo::world());
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(90, body_start)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                ui.pane(frame.buffer_mut(), "header", area, doc.clone(), false);
            })
            .unwrap();
        assert!(
            ui.visible_messages().is_empty(),
            "a header is not body consumption"
        );
    }

    #[tokio::test]
    async fn displayed_person_reply_changes_sender_status_without_reading_hidden_mail() {
        use super::super::view::{Body, Entry};
        use st3::model::ClaimInput;
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(st3::store::Store::open_memory("person-read").unwrap());
        for id in ["message/reply", "message/hidden"] {
            store
                .append_claim(&ClaimInput {
                    subject: id.into(),
                    kind: "message.sent".into(),
                    actor: Some("agent/example/keeper".into()),
                    fields: BTreeMap::from([
                        ("status".into(), serde_json::json!("sent")),
                        ("from".into(), serde_json::json!("agent/example/keeper")),
                        ("to".into(), serde_json::json!("person/avery")),
                        ("content".into(), serde_json::json!("Here is the reply.")),
                    ]),
                    evidence: vec![],
                    expected_subject: None,
                    idempotency_key: Some(id.into()),
                })
                .unwrap();
        }
        let state = st3::api::AppState {
            store: store.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: tokio::sync::watch::channel(0_u64).0,
            node: "person-read".into(),
            state_dir: root.path().into(),
            pty_root: root.path().join("pty"),
            pty_binary: root.path().join("unused-pty"),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: st3::model::PlannerSpec::default(),
        };
        let socket = root.path().join("daemon.sock");
        let server_path = socket.clone();
        let server = tokio::spawn(async move {
            st3::api::serve_unix(&server_path, st3::api::router(state))
                .await
                .unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = Client::unix_as(&socket, "person/avery");
        client.messages_get("message/reply").await.unwrap();
        assert_eq!(
            store.message("message/reply").unwrap().unwrap().status,
            "sent",
            "fetching does not acknowledge"
        );
        let mail = |id: &str| Entry {
            id: id.into(),
            at: "12:00".into(),
            body: Body::Mail {
                from: "keeper".into(),
                to: "you".into(),
                subject: "Reply".into(),
                body: "Here is the reply.".into(),
                delivered: false,
                dictated: false,
            },
        };
        let mut world = super::super::demo::world();
        let agent = world.agents.items()[0].clone();
        let agent_id = agent.id.clone();
        world.agents = Load::Ready(vec![agent]);
        world.conversations.insert(
            agent_id,
            Load::Ready(vec![
                mail("message/hidden"),
                Entry {
                    id: "filler".into(),
                    at: "12:00".into(),
                    body: Body::Assistant("A line of context.\n".repeat(60)),
                },
                mail("message/reply"),
            ]),
        );
        let mut ui = Ui::new(world);
        ui.switch_tab(1);
        assert!(
            ui.visible_messages().is_empty(),
            "cached conversations are not evidence"
        );
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| ui.render(frame)).unwrap();
        let shown = ui.visible_messages();
        assert!(shown.contains("message/reply"));
        assert!(
            !shown.contains("message/hidden"),
            "offscreen mail remains queued"
        );
        for id in shown {
            acknowledge_visible_message(&client, "person/avery", &id)
                .await
                .unwrap();
        }
        acknowledge_visible_message(&client, "person/avery", "message/reply")
            .await
            .unwrap();
        assert_eq!(
            store.message("message/reply").unwrap().unwrap().status,
            "read"
        );
        assert_eq!(
            store.message("message/hidden").unwrap().unwrap().status,
            "sent"
        );
        assert_eq!(
            store
                .claims_for("message/reply", Some("message.read"))
                .unwrap()
                .len(),
            1
        );
        let status: serde_json::Value = st3::client::Client::unix(&socket)
            .get("/v1/messages/delivery/message/reply")
            .await
            .unwrap();
        assert_eq!(
            status["delivery"]["state"], "read",
            "the sender sees the read receipt"
        );
        // Messages stay in conversations: Home never lists the unread one.
        assert!(
            client
                .attention_list(None, Some(50), false)
                .await
                .unwrap()
                .value
                .items
                .iter()
                .all(|item| match item {
                    Resource::Attention(item) => item.source_id != "message/hidden",
                    _ => true,
                })
        );
        assert_eq!(
            store.message("message/hidden").unwrap().unwrap().status,
            "sent",
            "an unread message stays unread"
        );
        ui.help = true;
        terminal.draw(|frame| ui.render(frame)).unwrap();
        assert!(
            ui.visible_messages().is_empty(),
            "covered bodies do not count"
        );
        server.abort();
    }

    #[test]
    fn a_conversation_without_its_transcript_is_one_failure_not_half_a_conversation() {
        let items: Vec<st3_client::TimelineEntry> = serde_json::from_value(serde_json::json!([
            {"id":"m","sequence":1,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"user","type":"message","final":true,
             "body":{"message_id":"message/one","from":"person/avery","to":"agent/example/harbor/keeper","title":"Status?"}},
            {"id":"c","sequence":2,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"user","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"How is the audit going?"}},
            {"id":"n","sequence":3,"revision":1,"timestamp":"2026-10-01T10:00:01Z","role":"system","type":"error","final":true,
             "body":{"code":"transcript-not-bound","message":"transcript not bound: the transcript could not be read: line 12: expected value","retryable":true,
                     "details":{"driver":"omp","transcript":"/srv/example/omp/sessions/harbor/0190.jsonl"}}},
        ]))
        .unwrap();
        let target = "agent/example/harbor/keeper";
        let shown = conversations(
            &Model::default(),
            "person/avery",
            &BTreeMap::from([(
                target.to_owned(),
                st3_conversation_ui::Timeline {
                    items,
                    ..Default::default()
                },
            )]),
            &BTreeMap::new(),
            &[target.to_owned()],
        );
        match &shown[target] {
            Load::Failed(reason) => assert!(
                reason.contains("line 12: expected value") && reason.contains("0190.jsonl"),
                "{reason}"
            ),
            other => panic!("expected one failure, not entries: {other:?}"),
        }
    }

    fn fixture_screen() -> st3_client::TerminalScreen {
        let text = include_str!("../../../../docs/st3/client-v0/fixtures/terminal-screen.json");
        let envelope: serde_json::Value = serde_json::from_str(text).unwrap();
        serde_json::from_value(envelope["value"].clone()).unwrap()
    }

    fn plain(line: &ratatui::text::Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn screen_lines_mark_cut_lines_hide_redacted_ones_and_fall_back_to_text() {
        let mut screen = fixture_screen();
        let styled = screen_lines(&screen);
        assert_eq!(plain(&styled[0]), "$ cargo build");
        assert!(styled[0].spans.len() > 1, "runs keep their styles");
        screen.lines[0].truncated = true;
        screen.lines[1].runs.clear();
        screen.lines[2].redacted = true;
        let lines = screen_lines(&screen);
        assert_eq!(plain(&lines[0]), "$ cargo build…");
        assert_eq!(plain(&lines[1]), "Finished");
        assert_eq!(plain(&lines[2]), "[redacted]");
    }
}

/// A direct stream to a terminal's PTY session, and what it is.
struct Direct {
    name: String,
    incarnation: String,
    stream: std::os::unix::net::UnixStream,
}

/// A direct stream to `subject`'s PTY session: st's raw terminal stream, fenced to the
/// terminal's incarnation and routed to the host that owns it. `subject` is an agent (its
/// terminal is the first of `runtime_ids` that has one) or a shell's terminal. With `expected`,
/// only that incarnation: a terminal that restarted since is not quietly swapped in.
async fn attach_direct(
    client: &Client,
    subject: &str,
    runtime_ids: &[String],
    expected: Option<&str>,
) -> Result<Direct, String> {
    let mut found = Vec::new();
    if subject.starts_with("terminal/") {
        // A shell just started may take a moment before its screen exists.
        let mut tries = 0;
        let screen = loop {
            match client.terminal_screen(subject).await {
                Ok(screen) => break screen,
                Err(_) if tries < 10 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(error) => return Err(error.plain()),
            }
        };
        found.push((
            subject.trim_start_matches("terminal/").replace('/', "."),
            subject.to_owned(),
            screen.value.runtime_incarnation,
        ));
    } else {
        for id in runtime_ids {
            let Ok(envelope) = client.runtimes_get(id).await else {
                continue;
            };
            let Resource::Runtime(runtime) = envelope.value else {
                continue;
            };
            if let (Some(terminal), Some(incarnation)) =
                (runtime.terminal_id, runtime.incarnation_id)
            {
                found.push((runtime.runtime_id, terminal, incarnation));
            }
        }
    }
    let mut reason = "the agent has no terminal right now".to_owned();
    for (name, terminal, incarnation) in found {
        if expected.is_some_and(|expected| expected != incarnation) {
            reason = "the terminal restarted; Ctrl+] attaches the new one".into();
            continue;
        }
        let attachment = match client
            .raw_terminal_attachment(&terminal, &incarnation, st3_client::RawTerminalMode::Attach)
            .await
        {
            Ok(attachment) => attachment,
            Err(error) => {
                reason = error.plain();
                continue;
            }
        };
        match client.raw_terminal_stream(&attachment).await {
            Ok(stream) => {
                return stream
                    .into_std()
                    .map(|stream| Direct {
                        name,
                        incarnation,
                        stream,
                    })
                    .map_err(|error| error.to_string());
            }
            Err(error) => reason = error.plain(),
        }
    }
    Err(reason)
}
