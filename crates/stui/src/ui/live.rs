//! `stui`: the screens on the live graph.
//!
//! The feed keeps the attention, missions and agents windows current over one socket, joined
//! by st, and follows the open terminal on the same socket. This loop turns those windows
//! into a `World`, fetches what only the selected item or tab needs (a conversation, a launch
//! preview, the fleet), and carries out the actions the screens queue. A refresh replaces
//! data in place: it never empties a list or a conversation while the fresh copy is on its way.

use super::adapt::{self, Extras};
use super::direct;
use super::glass::GlassWrite;
use super::view::{Load, MissionPreview};
use super::{AgentControl, Effect, Guard, Ui};
use crate::feed::{self, Command, Inventories, Inventory, TerminalUpdate, Window};
use crate::model::{self, Collection, Model};
use anyhow::Result;
use crossterm::{
    event::Event,
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

    /// The conversation drew it, noted once for the quality counters.
    echoed: bool,
    /// When it was last sent (or sent again): one st has not answered for a while is said to be
    /// unconfirmed, so it can be sent again or cleared rather than wait forever.
    since: Instant,
}

/// How often st is asked how long a message it takes, and how soon again after it did not answer.
const MESSAGE_LIMIT_REREAD: Duration = Duration::from_secs(600);
const MESSAGE_LIMIT_RETRY: Duration = Duration::from_secs(30);
/// The followed windows arrive over the live stream, whose frames carry no `sync` notice, so a
/// small Attention page read supplies it: once when st connects, before the first frames, and
/// again only when a later Attention or Agents frame shows the graph moving, at most once a
/// minute. An idle screen sends no request (see client_only's idle oracle). Remove when every
/// supported daemon streams the notice.
const SYNC_MIN_GAP: Duration = Duration::from_secs(60);

/// How long a message waits for st's answer before it says st has not confirmed it.
/// A draw slower than this, or a whole turn of the main loop (everything it did before waiting for
/// input) slower than that, is written to slow.log with what was on screen.
const SLOW_DRAW: Duration = Duration::from_millis(120);
const SLOW_TURN: Duration = Duration::from_millis(250);
const UNANSWERED_AFTER: Duration = Duration::from_secs(30);

/// A message request as sent: st keys its receipt on the whole request.
#[derive(Clone, Debug)]
struct Sent {
    id: String,
    idempotency_key: String,
    snapshot_id: String,
    /// What it said, as sent: rebuilt later, it could name the agent's newer session, and st
    /// refuses a repeat that differs as a conflict.
    parameters: MessageSendParameters,
}

/// Read evidence belongs to a presented frame, so navigation cannot cancel retries.
#[derive(Default)]
struct ReadReceipts {
    confirmed: HashSet<String>,
    pending: BTreeMap<String, Option<Instant>>,
    /// How many times in a row each failed: the wait before the next try grows with it.
    failures: BTreeMap<String, u32>,
    /// Refused for good (st will not take this person's read of it): never asked again.
    refused: HashSet<String>,
}

/// How a read receipt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReceiptOutcome {
    Done,
    /// Try again later: the connection or the graph moved.
    Retry,
    /// st refused it and said it would refuse again.
    Refused,
}

/// Whether a failed read receipt is worth trying again: a refusal st marks as not retryable
/// (this identity may not read it) is final, except a stale fence, which a fresh one answers.
fn receipt_outcome(error: &anyhow::Error) -> ReceiptOutcome {
    use st3_client::{ClientError, ErrorCode};
    match error.downcast_ref::<ClientError>() {
        Some(ClientError::Api(code, _, envelope))
            if !envelope.retryable && !matches!(code, ErrorCode::StaleFence) =>
        {
            ReceiptOutcome::Refused
        }
        _ => ReceiptOutcome::Retry,
    }
}

impl ReadReceipts {
    fn displayed(&mut self, id: String, now: Instant) {
        if !self.confirmed.contains(&id) && !self.refused.contains(&id) {
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
    fn completed(&mut self, id: String, outcome: ReceiptOutcome, now: Instant) {
        match outcome {
            ReceiptOutcome::Done => {
                self.pending.remove(&id);
                self.failures.remove(&id);
                if self.confirmed.len() >= 2048 {
                    self.confirmed.clear();
                }
                self.confirmed.insert(id);
            }
            ReceiptOutcome::Refused => {
                self.pending.remove(&id);
                self.failures.remove(&id);
                if self.refused.len() >= 2048 {
                    self.refused.clear();
                }
                self.refused.insert(id);
            }
            ReceiptOutcome::Retry => {
                let tries = self.failures.entry(id.clone()).or_insert(0);
                *tries = tries.saturating_add(1);
                // 2 s, 4 s, 8 s ... up to a minute: a message st keeps failing is not asked every
                // few seconds.
                let wait = Duration::from_secs((2_u64 << (*tries - 1).min(5)).min(60));
                self.pending.insert(id, Some(now + wait));
            }
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
    /// `st ui --glasses` / `--glass NAME`: open glasses instead of the sidebar layout, at the
    /// named glass or the last one used on this device.
    pub glass: Option<Option<String>>,
    pub(crate) open_timing: Option<super::open_timing::Trace>,
    pub(crate) process_started: Instant,
    pub initial_subject: Option<String>,
    /// Where seats on this machine leave requests to show something; none for a paired device.
    pub requests: Option<std::path::PathBuf>,
}

// Setup's initial destination waits for a live inventory, rather than cached agent data.
// Consume it after the first ready inventory so later updates leave the person's focus alone.
pub(super) fn focus_initial_subject(
    ui: &mut Ui,
    subject: &mut Option<String>,
    agents_live: bool,
    glasses_ready: bool,
) {
    if !agents_live || !glasses_ready || !matches!(ui.world.link, super::view::Link::Live) {
        return;
    }
    let Some(id) = subject.take() else {
        return;
    };
    if !ui.world.agents.items().iter().any(|agent| agent.id == id) {
        return;
    }
    ui.open(&id);
}

/// Open what seats asked for with `st ui open`, once the inventory is live: an agent a seat has
/// only just made is waited for, and one that never appears is answered as not found.
pub(super) fn answer_open_requests(
    ui: &mut Ui,
    inbox: &mut super::requests::Inbox,
    ready: bool,
) {
    inbox.read();
    if !ready || !matches!(ui.world.link, super::view::Link::Live) {
        return;
    }
    let waiting = inbox
        .waiting()
        .map(|(request, since)| (request.clone(), since.elapsed()))
        .collect::<Vec<_>>();
    for (request, waited) in waiting {
        let Ok(pane) = super::requests::pane_for(&request.subject) else {
            inbox.answer(&request.id, &super::requests::Answer::NotFound);
            continue;
        };
        if ui.inventory_has(&pane) {
            let answer = ui.open_requested(
                pane,
                request.place,
                request.keep_focus,
                request.from.as_deref(),
            );
            inbox.answer(&request.id, &answer);
        } else if waited >= super::requests::GRACE {
            inbox.answer(&request.id, &super::requests::Answer::NotFound);
        }
    }
}

fn cancel_initial_subject(subject: &mut Option<String>, input: &Event) {
    if matches!(input, Event::Key(_) | Event::Paste(_))
        || matches!(input, Event::Mouse(mouse) if mouse.kind != crossterm::event::MouseEventKind::Moved)
    {
        *subject = None;
    }
}

/// The terminal the feed follows for the open terminal view.
struct Following {
    terminal_id: String,
    attachment_id: String,
    incarnation: String,
}

/// How long the missions window stays followed after a mission list or card leaves the screen.
const MISSIONS_GRACE: Duration = Duration::from_secs(30);

#[derive(Default)]
struct InventoryGrace(BTreeMap<Inventory, Instant>);
impl InventoryGrace {
    fn followed(&mut self, visible: Inventories, now: Instant) -> Inventories {
        // A changed period replaces the old usage slot immediately; leaving the screen keeps
        // its last period through the same grace as the other windows.
        if let Some(hours) = visible.usage_hours {
            self.0.retain(|inventory, _| !matches!(inventory, Inventory::Usage(old) if *old != hours));
        }
        for inventory in visible.windows() { self.0.insert(inventory, now + MISSIONS_GRACE); }
        self.0.retain(|_, until| *until > now);
        let mut held = Inventories::default();
        for inventory in self.0.keys() {
            match inventory {
                Inventory::Clients => held.clients = true, Inventory::Usage(hours) => held.usage_hours = Some(*hours),
                Inventory::Machines => held.machines = true, Inventory::Devices => held.devices = true,
                Inventory::Sessions => held.sessions = true, Inventory::Terminals => held.terminals = true,
            }
        }
        held
    }
}

enum InventoryRead {
    Rows(Inventory, Result<Collection, String>),
    Terminals(Result<(Vec<Resource>, bool), String>),
    Clients(Result<st3_client::ClientConnections, String>),
    Usage(u64, Result<st3_client::UsagePeriod, String>),
}
impl InventoryRead {
    fn inventory(&self) -> Inventory {
        match self { Self::Rows(inventory, _) => *inventory, Self::Terminals(_) => Inventory::Terminals,
            Self::Clients(_) => Inventory::Clients, Self::Usage(hours, _) => Inventory::Usage(*hours) }
    }
}

async fn legacy_inventory(client: &Client, inventory: Inventory) -> InventoryRead {
    match inventory {
        Inventory::Machines => InventoryRead::Rows(inventory, model::read_machines(client).await.map_err(|error| error.to_string())),
        Inventory::Devices => InventoryRead::Rows(inventory, model::read_devices(client).await.map_err(|error| error.to_string())),
        Inventory::Sessions => InventoryRead::Rows(inventory, model::read_native_sessions(client).await.map_err(|error| error.to_string())),
        Inventory::Terminals => InventoryRead::Terminals(client.terminals_list(None, Some(200), false).await.map(|reply| (reply.value.items, reply.value.page.has_more)).map_err(|error| error.plain())),
        Inventory::Clients => InventoryRead::Clients(client.clients_list().await.map(|reply| reply.value).map_err(|error| match error {
            ClientError::Api(st3_client::ErrorCode::NotFound, ..) => "this st does not list its clients yet: its daemon needs an update".into(),
            error => error.plain(),
        })),
        Inventory::Usage(hours) => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            InventoryRead::Usage(hours, client.usage_period(Some(now.saturating_sub(hours.saturating_mul(3_600_000))), None).await.map(|reply| reply.value).map_err(|error| usage_error(&error)))
        }
    }
}

fn terminal_rows(items: Vec<Resource>) -> Vec<super::resource_sidebar::ResourceRow> {
    items.into_iter().filter_map(|item| match item {
        Resource::Runtime(runtime) => runtime.terminal_id.map(|terminal| super::resource_sidebar::ResourceRow {
            id: runtime.header.id, kind: "Terminals".into(), title: runtime.owner_id, open: terminal, missing: false,
        }),
        _ => None,
    }).collect()
}

/// A polled inventory's read finished at `now` (older daemons serve no inventory windows).
/// Its error is held until it has kept failing for the time the feed waits before reporting a
/// window (`feed::REPORT_AFTER`); a read that succeeds clears both the clock and the error.
fn note_legacy_read(
    failing: &mut BTreeMap<&'static str, Instant>,
    errors: &mut BTreeMap<String, String>,
    name: &'static str,
    error: Option<&String>,
    now: Instant,
) {
    match error {
        Some(error) => {
            let since = *failing.entry(name).or_insert(now);
            if now.duration_since(since) >= feed::REPORT_AFTER {
                errors.insert(name.into(), error.clone());
            }
        }
        None => {
            failing.remove(name);
            errors.remove(name);
        }
    }
}

fn window_label(window: Window) -> String {
    match window { Window::Inventory(inventory) => inventory.collection().into(), _ => format!("{window:?}") }
}

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

/// A disappearing card can race the action's reply (including a refusal). Count
/// landing only when both the action succeeded and a fresh window showed it gone.
struct AnswerTiming {
    sent: Instant,
    accepted: bool,
    gone: bool,
    seen: bool,
}
impl AnswerTiming {
    fn new(sent: Instant) -> Self {
        Self { sent, accepted: false, gone: false, seen: false }
    }
    fn landed(&self) -> Option<Instant> {
        (self.accepted && self.gone).then_some(self.sent)
    }
}

enum Fetched {
    Content(super::content::Key, Result<super::content::Fetched, String>),
    /// An image's terminal encoding, built on a worker for the size a draw asked.
    Protocol(super::content::Key, u16, u16, super::content::Encoding),
    Read(String, ReceiptOutcome),
    /// A page before the oldest entry of a conversation's session: its entries, whether st
    /// holds more before them, and the cursor for that next page; or why it could not be read.
    Older {
        target: String,
        session_id: String,
        page: Result<OlderPage, String>,
        /// How long st took to answer, so a slow st is not asked for more than the person scrolled to.
        took: Duration,
    },
    Preview(String, Load<MissionPreview>),
    /// The message behind an unread-message item: sender, title and text.
    Body(String, String, Option<String>, String),
    Notice(String),
    AlertOutcome(String, bool),
    /// Harness sessions st did not start, found on this machine.
    Session(String, Result<Box<st3_client::Session>, String>),
    /// The Fleet tab's machines and paired devices.
    Observations { generation: u64, outcome: Result<(Vec<st3_client::ResourceObservation>, bool), String> },
    Inventory { generation: u64, result: InventoryRead },
    /// st's conversation search for the palette's query, or why st could not say.
    Said(String, Result<st3_client::ConversationSearch, String>),
    /// A send finished: the pending token and st's message id, or why it failed and whether st's
    /// answer is unknown.
    Sent(String, Result<Option<String>, (String, bool)>),
    /// The longest message st says it takes, in UTF-8 bytes; None when it does not say.
    MessageLimit(Result<Option<usize>, ()>),
    /// The `sync` notice on a one-row Attention page: None when st says nothing is missing.
    Sync(Result<Option<st3_client::SyncNotice>, ()>),
    /// st started an agent asked for here.
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
    /// A shell's attach was refused for a reason that passes; it is being tried again.
    AttachWaiting { agent: String, reason: String },
    /// st gave no direct stream; follow its view of the terminal instead.
    NativeFailed {
        agent: String,
        runtime_ids: Vec<String>,
        reason: String,
    },
    /// st answered a glass write: the glass, the write's key, and the revision it accepted.
    /// An error carries whether st refused it because the Sidebar was retired.
    SidebarSaved { write: super::resource_sidebar::Write, outcome: Result<(), (String, bool)> },
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
        mut open_timing,
        process_started,
        mut initial_subject,
        requests,
    } = context;
    let (fetched_tx, fetched) = mpsc::channel::<Fetched>();
    // What seats ask this terminal to show (`st ui open`); a paired device has no seats here.
    let mut open_requests = super::requests::Inbox::new(requests);
    let mut model = cached.unwrap_or_default();
    let mut agents_live = false;
    // The feed announces a granted glasses window before sending any core window.
    let mut glasses_ready = true;
    // The attention window is already this person's; nothing else names the actor.
    model.actor = person.clone();
    let mut extras = Extras::default();
    // Each conversation st has sent, kept after it closes so reopening it shows its last entries.
    let mut timelines: BTreeMap<String, st3_conversation_ui::Timeline> = BTreeMap::new();
    let mut failed: BTreeMap<String, String> = BTreeMap::new();
    // How many earlier pages each conversation read on its own to fill its first screen.
    let mut filled: BTreeMap<String, usize> = BTreeMap::new();
    // The agent or session whose conversation the feed holds.
    let mut conversing: Vec<String> = Vec::new();
    let mut opens: BTreeMap<String, Instant> = BTreeMap::new();
    let mut terminal_opens: BTreeMap<String, Instant> = BTreeMap::new();
    let mut answers: BTreeMap<String, AnswerTiming> = BTreeMap::new();
    let mut landed_answers: Vec<Instant> = Vec::new();
    // The session each conversation was last subscribed again for, so it is asked once.
    let mut resubscribed: BTreeMap<String, String> = BTreeMap::new();
    let mut sessions_requested = HashSet::new();
    let mut preview_requested: HashSet<String> = HashSet::new();
    let mut body_requested: HashSet<String> = HashSet::new();
    let mut read_receipts = ReadReceipts::default();
    // Messages sent from here, shown at once until st reports them back.
    let mut pending: Vec<Pending> = Vec::new();
    let mut ui = Ui::new(adapt::world(&model, &person, &extras));
    ui.load_prefs();
    ui.drafts_path = super::drafts_store::path();
    let (_run, earlier) = super::lastrun::begin(&crate::version::short(crate::version::now()));
    ui.load_drafts();
    ui.quality = super::slo::Draw::new(process_started);
    if earlier.is_some() {
        super::slo::METRICS.add("drafts_restored_unclean", ui.drafts_saved.len() as u64);
    }
    if glass.is_some() {
        ui.resource_sidebar.load(&person);
    }
    ui.resource_sidebar.observe(&ui.world);
    ui.build = true;
    ui.live = true;
    ui.glasses = glass.map(|name| {
        super::glass::Glasses::open(name, super::glass_store::path(&person))
    });
    // A glass opens where this device left it.
    ui.show_focused();
    if ui.glasses.is_none() {
        ui.flash(
            "The classic layout is going away: plain st opens spaces, with Ctrl+S for this list",
        );
    } else if std::env::args().any(|arg| arg == "--old") {
        ui.flash("The old screens are gone: this is spaces, and ? shows its keys");
    }

    // A stui killed from outside leaves its last picture on the screen; the next one says so.
    if let Some(note) = earlier {
        // Long enough to read: the usual flash ends after four seconds.
        ui.flash = Some((note, Instant::now() + Duration::from_secs(26)));
    }
    let _guard = Guard::enter(ui.glasses.is_some())?;
    // The release smoke test's probe: the terminal is restored after a panic too.
    #[cfg(debug_assertions)]
    if std::env::var_os("STUI_TEST_PANIC_AFTER_ENTER").is_some() {
        panic!("terminal restoration probe");
    }
    // How images are drawn: asked of the terminal itself, once, inside the alternate screen and
    // before any event is read, as ratatui-image asks it (kitty graphics, sixel in its device
    // attributes, iTerm2, through tmux too). Only a terminal that answered a status report is
    // asked: one that never answers would leave the query reading stdin and swallow keys, so it
    // gets half blocks unasked. Images load by themselves only where the terminal draws them.
    let answered = super::attach::terminal_answers(Duration::from_millis(400));
    let picker = if answered {
        ratatui_image::picker::Picker::from_query_stdio()
            .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks())
    } else {
        ratatui_image::picker::Picker::halfblocks()
    };
    ui.auto_images =
        picker.protocol_type() != ratatui_image::picker::ProtocolType::Halfblocks;
    ui.picker = Some(picker);
    // Clickable addresses (OSC 8) where the terminal draws images, as kitty and its kin do.
    let hyperlinks = super::attach::graphics_terminal();
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let started = Instant::now();
    let mut changed = true;
    let mut offline_drawn = false;
    // When st is next asked how long a message it takes: on connecting, then now and then, since
    // a daemon that is upgraded changes it.
    let mut limit_next = Instant::now();
    let stopping = super::stop_flag()?;
    let mut attached: Option<Following> = None;
    // The runtimes of the agent whose terminal view is open, to follow it again after a pause.
    let mut terminal_runtimes: Option<Vec<String>> = None;
    let mut terminal_runtimes_by_agent: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // The fleet host that owns each opened terminal, so a dropped stream attaches again directly.
    let mut terminal_owners: BTreeMap<String, String> = BTreeMap::new();
    // When a terminal was asked for, while it connects: the wait is shown, and a long one named.
    let mut attach_started: Option<Instant> = None;
    // Why the attach in progress is waiting, when st refused it for a reason that passes.
    let mut attach_note: Option<String> = None;
    // The cursor shape last set, so it changes only when the attached terminal asks.
    let mut cursor_style: Option<crossterm::cursor::SetCursorStyle> = None;
    // The tab shown on the last pass: opening a tab loads what only it needs.
    let mut shown_tab = usize::MAX;
    let mut resources_shown = false;
    let mut resource_pane = None;
    let mut resources_stale = true;
    let mut resources_reading = false;
    let mut resources_generation = 0_u64;
    // Whether the feed was last told to follow the missions window, and until when the last time
    // one was on screen keeps it followed.
    let mut missions_sent = false;
    let mut missions_until: Option<Instant> = None;
    let mut content_reads = super::content::Reads::default();
    // Image encodings being built on a worker, by image and size, so each is built once.
    let mut encoding: std::collections::BTreeSet<(super::content::Key, u16, u16)> =
        std::collections::BTreeSet::new();
    let mut inventory_grace = InventoryGrace::default();
    let mut inventory_sent: Option<Inventories> = None;
    let mut usage_wanted_hours = None;
    let mut legacy_requested = BTreeSet::new();
    // When each polled inventory first failed on this run of failures: a read that fails for a
    // moment (a daemon catching up) is not said; one that keeps failing is.
    let mut legacy_failing: BTreeMap<&'static str, Instant> = BTreeMap::new();
    let mut legacy_pending = BTreeSet::new();
    // The palette's conversation search: what st was last asked, and what is typed since when.
    let mut said_asked: Option<String> = None;
    let mut said_typed: Option<(String, Instant)> = None;
    let mut last_cache_save = Instant::now();
    // The notice the page read last found, carried onto every window the stream rebuilds.
    let mut page_sync: Option<st3_client::SyncNotice> = None;
    let mut sync_read_at: Option<Instant> = None;
    // When a read is wanted, because a frame after the first showed the graph moving.
    let mut sync_due: Option<Instant> = None;
    // The windows whose first frame has arrived on this connection: that frame is not a change.
    let mut windows_seen: BTreeSet<String> = BTreeSet::new();
    // A closed terminal ends the loop: without this check a detached stui spins and keeps
    // polling the daemon forever.
    while !ui.quit
        && !stopping.load(std::sync::atomic::Ordering::Relaxed)
        && !crate::stdin_hung_up()
    {
        let turn_started = Instant::now();
        // A program written over on disk while it runs is told to the person at once, and noted.
        if started.elapsed().as_secs().is_multiple_of(10) && super::lastrun::binary_replaced() {
            ui.flash("st was updated on disk while this was running: restart it, the system may stop it");
        }
        ui.tick = (started.elapsed().as_millis() / 100) as u64;
        if ui
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(4))
        {
            ui.flash = None;
        }
        // A terminal still connecting says how long it has waited, so a slow st does not look
        // like a stuck screen (Nathan, 2026-10-05: attaching took a long time).
        if let (Some(started), Some(view)) = (attach_started, ui.terminal.as_mut())
            && view.native.is_none()
            && view.ended.is_none()
            && view
                .stale
                .as_deref()
                .is_some_and(|stale| stale.starts_with("connecting"))
        {
            view.stale = Some(match &attach_note {
                Some(reason) => format!("{} · {reason}, trying again", connecting_text(started.elapsed())),
                None => connecting_text(started.elapsed()),
            });
        }
        if extras.live && Instant::now() >= limit_next {
            limit_next = Instant::now() + MESSAGE_LIMIT_RETRY;
            let client = client.clone();
            let tx = fetched_tx.clone();
            runtime.spawn(async move {
                let limit = client
                    .capabilities()
                    .await
                    .map(|found| found.value.limits.max_message_bytes)
                    .map_err(|_| ());
                let _ = tx.send(Fetched::MessageLimit(limit));
            });
        }
        if sync_due.is_some_and(|due| Instant::now() >= due) && extras.live && extras.offline.is_none() {
            sync_due = None;
            sync_read_at = Some(Instant::now());
            read_sync(&runtime, &client, &fetched_tx);
        }
        while let Ok(update) = incoming.try_recv() {
            match update {
                feed::Update::GlassesVersion(version) => {
                    super::set_glasses_version(version);
                    glasses_ready = ui.glasses.is_none();
                }
                feed::Update::Connected(member) => {
                    limit_next = Instant::now();
                    // Another member may say something else: drop the old notice and read this
                    // member's once, ahead of its first frames.
                    page_sync = None;
                    fill_notice(&mut model, None);
                    windows_seen.clear();
                    sync_due = None;
                    sync_read_at = Some(Instant::now());
                    read_sync(&runtime, &member, &fetched_tx);
                    ui.quality.live(false);
                    agents_live = false;
                    glasses_ready = true;
                    client = member;
                    resources_generation += 1;
                    resources_stale = true;
                    extras.inventory_supported = None;
                    extras.inventory_partial.clear();
                    inventory_sent = None; legacy_requested.clear(); legacy_pending.clear();
                    extras.live = false;
                    attached = None;
                    shown_tab = usize::MAX;
                    sessions_requested.clear();
                    preview_requested.clear();
                    body_requested.clear();
                    changed = true;
                }
                feed::Update::InventorySupport { granted, member_build } => {
                    extras.inventory_supported = Some(granted);
                    model.member_build = member_build;
                    changed = true;
                }
                feed::Update::Window { window: Window::Arrangements, items, .. } => {
                    ui.resource_sidebar.writable = true;
                    ui.resource_sidebar.arrangements = items.into_iter().filter_map(|item| match item {
                        Resource::Arrangement(arrangement) if arrangement.owner == person && !arrangement.deleted => Some(arrangement),
                        _ => None,
                    }).collect();
                    if ui.resource_sidebar.arrangements.iter().any(|a| a.body.folders.values().any(|folder| folder.tombstone.is_none()))
                        && let Some(glasses) = ui.glasses.as_mut()
                        && !glasses.sidebar.configured {
                        glasses.sidebar.shown = true;
                    }
                    changed = true;
                }
                feed::Update::Window {
                    window: Window::Glasses,
                    items,
                    ..
                } => {
                    glasses_ready = true;
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
                    received,
                    window,
                    snapshot,
                    items,
                    has_more,
                } => {
                    if matches!(window, Window::Attention | Window::Agents) {
                        ui.quality
                            .frame(received, if window == Window::Attention { 1 } else { 2 });
                    }
                    if window == Window::Attention && !has_more {
                        // Reuse the pending-answer index: one lookup per card, rather
                        // than scanning the whole window separately for every answer.
                        for timing in answers.values_mut() { timing.seen = false; }
                        for item in &items {
                            if let Resource::Attention(card) = item
                                && let Some(timing) = answers.get_mut(&card.header.id) {
                                timing.seen = card.state != "resolved";
                            }
                        }
                        answers.retain(|_, timing| {
                            if !timing.seen { timing.gone = true; }
                            if let Some(at) = timing.landed() {
                                landed_answers.push(at);
                                false
                            } else {
                                true
                            }
                        });
                    }
                    extras.window_errors.remove(&window_label(window));
                    if matches!(window, Window::Attention | Window::Agents) {
                        let first = windows_seen.insert(window_label(window));
                        sync_due = sync_due_after_frame(first, sync_read_at, sync_due, Instant::now());
                    }
                    if window == Window::Summary {
                        extras.summary = items.iter().find_map(|item| match item {
                            Resource::Summary(summary) => Some(summary.clone()),
                            _ => None,
                        });
                        changed = true;
                        continue;
                    }
                    // Back in touch: glass changes st has not confirmed go again, same keys.
                    if !extras.live {
                        for write in ui.unsent_glass_writes() {
                            save_glass(&runtime, &client, &fetched_tx, write);
                        }
                    }
                    if let Window::Inventory(inventory) = window {
                        extras.inventory_partial.retain(|name| name != inventory.collection());
                        if has_more { extras.inventory_partial.push(inventory.collection().into()); }
                    }
                    let collection = window_collection(items, snapshot, has_more, page_sync.as_ref());
                    match window {
                        Window::Attention => {
                            model.now = collection;
                            extras.attention_live = true;
                        }
                        Window::Missions => {
                            model.missions = collection;
                            extras.missions_followed = true;
                        }
                        Window::Agents => {
                            model.agents = collection;
                            extras.agents_live = true;
                            agents_live = true;
                        }
                        Window::Inventory(inventory) => match inventory {
                            Inventory::Machines => model.machines = collection,
                            Inventory::Devices => model.devices = collection,
                            Inventory::Sessions => model.sessions = collection,
                            Inventory::Terminals => {
                                ui.resource_sidebar.terminals = terminal_rows(collection.items);
                                ui.resource_sidebar.terminals_has_more = collection.truncated;
                            }
                            Inventory::Clients => model.clients = Some(Ok(model::inventory_clients(collection.items, collection.snapshot.as_ref().unwrap().host_id.clone()))),
                            Inventory::Usage(hours) if hours == ui.usage_hours => model.usage = Some(model::inventory_usage(collection.items)),
                            Inventory::Usage(_) => {},
                        },
                        Window::Glasses | Window::Summary | Window::Arrangements => {}
                    }
                    extras.live = true;
                    extras.offline = None;
                    extras.offline_since = None;
                    extras.degraded = None;
                    model.last_connected =
                        Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                    changed = true;
                }
                feed::Update::Conversation {
                    target,
                    session_id,
                    replace,
                    has_more,
                    items,
                    header,
                } => {
                    if let Some(trace) = &mut open_timing {
                        trace.received(&target, items.len());
                    }
                    if opens.contains_key(&target)
                        && adapt::unreadable_transcript(&items).is_some()
                        && opens.remove(&target).is_some()
                    {
                        super::slo::METRICS.add("open_not_bound", 1);
                    }
                    failed.remove(&target);
                    if replace
                        && !timelines
                            .get(&target)
                            .is_some_and(|timeline| timeline.older.paged)
                    {
                        filled.remove(&target);
                    }
                    ui.conversation_updated(&target);
                    if let Some(header) = &header {
                        ui.conversation_headers
                            .insert(target.clone(), header.clone());
                    }
                    timelines
                        .entry(target)
                        .or_default()
                        .apply(st3_conversation_ui::Frame {
                            replace,
                            has_more,
                            items,
                            session_id: Some(session_id),
                            header,
                        });
                    // A message sent from here is done once st shows it in the conversation.
                    pending.retain(|pending| {
                        let Some(id) = pending.message_id.as_ref() else {
                            return true;
                        };
                        let shown = timelines.values().flat_map(|timeline| &timeline.items).any(|entry| {
                            matches!(&entry.body, TimelineBody::Message(message) if &message.message_id == id)
                        });
                        if shown {
                            super::slo::METRICS.observe("send_visible_ms", pending.since.elapsed());
                        }
                        !shown
                    });
                    changed = true;
                }
                feed::Update::ConversationFailed {
                    target,
                    message,
                    permanent,
                    code,
                } => {
                    if opens.remove(&target).is_some() {
                        super::slo::METRICS.add(
                            match code {
                                Some(st3_client::ErrorCode::RemoteUnavailable) => {
                                    "open_remote_unavailable"
                                }
                                _ => "open_cannot_load",
                            },
                            1,
                        );
                    }
                    // A permanent refusal is said once, without "retrying".
                    if !permanent {
                        ui.conversation_failed(&target, &message);
                    } else {
                        ui.stalled.remove(&target);
                    }
                    failed.insert(target, message);
                    changed = true;
                }
                feed::Update::WindowStopped(window) => {
                    if let Window::Inventory(inventory) = window {
                        extras.window_errors.remove(&window_label(window));
                        extras.inventory_partial.retain(|name| name != inventory.collection());
                        changed = true;
                    }
                    // A window left on purpose (no Missions list on screen): what was held for it
                    // is out of date and not shown as current.
                    if window == Window::Missions {
                        model.missions = Collection::default();
                        extras.missions_followed = false;
                        changed = true;
                    }
                }
                feed::Update::WindowFailed(window, error) => {
                    if let Window::Inventory(Inventory::Clients) = window && model.clients.is_none() { model.clients = Some(Err(error.clone())); }
                    if let Window::Inventory(Inventory::Usage(hours)) = window && hours == ui.usage_hours && model.usage.is_none() { model.usage = Some(Err(error.clone())); }
                    // Not flashed: a window that fails is retried quietly and the last good view
                    // stays. What is left to say (it has not loaded, or has stayed unserved past
                    // `REPORT_AFTER`, which the feed waits before sending this) goes where the
                    // window is shown.
                    extras.window_errors.insert(window_label(window), error);
                    changed = true;
                }
                feed::Update::Degraded(reason) => {
                    ui.quality.live(false);
                    // st answered; only its live stream is being replaced. Not offline, and
                    // nothing on screen is cleared.
                    extras.live = false;
                    extras.offline = None;
                    extras.offline_since = None;
                    ui.flash(format!(
                        "Live stream lost ({reason}); st answers, reconnecting"
                    ));
                    extras.degraded = Some(reason);
                    attached = None;
                    changed = true;
                }
                feed::Update::Offline(error) => {
                    ui.quality.live(false);
                    extras.live = false;
                    extras.degraded = None;
                    extras.offline = Some(error);
                    extras.offline_since.get_or_insert_with(Instant::now);
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
                        if ui
                            .terminal
                            .as_ref()
                            .is_some_and(|view| view.ended.is_none())
                        {
                            super::slo::METRICS.add("dead_terminal_tabs", 1);
                        }
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
                Fetched::Content(key, result) => {
                    content_reads.complete(&key);
                    ui.content.finish(key, result);
                }
                Fetched::Protocol(key, width, height, protocol) => {
                    encoding.remove(&(key.clone(), width, height));
                    ui.content.install_protocol(key, width, height, protocol);
                }
                Fetched::Read(id, result) => {
                    read_receipts.completed(id, result, Instant::now());
                }
                // A failed read keeps what was last found; the next try is the next minute.
                Fetched::Sync(Ok(notice)) => {
                    page_sync = notice;
                    fill_notice(&mut model, page_sync.as_ref());
                }
                Fetched::Sync(Err(())) => {}
                Fetched::MessageLimit(limit) => match limit {
                    Ok(limit) => {
                        ui.message_limit = limit;
                        limit_next = Instant::now() + MESSAGE_LIMIT_REREAD;
                    }
                    // Asked again soon; the last answer stands meanwhile.
                    Err(()) => limit_next = Instant::now() + MESSAGE_LIMIT_RETRY,
                },
                Fetched::Sent(token, outcome) => {
                    let mut restored = false;
                    if let Some(entry) = pending.iter_mut().find(|entry| entry.token == token) {
                        match outcome {
                            Ok(id) => {
                                super::slo::METRICS.observe("send_ack_ms", entry.since.elapsed());
                                entry.message_id = id;
                                entry.failed = None;
                                entry.unconfirmed = false;
                            }
                            Err((error, unconfirmed)) => {
                                // st said no, or never took it: the text goes back to the box
                                // for the person to change, unless something newer is there. A
                                // send whose answer was lost stays here as unconfirmed, so it is
                                // never sent twice.
                                if !unconfirmed
                                    && let Effect::Send {
                                        agent,
                                        text,
                                        in_reply_to,
                                        tags,
                                        images,
                                    } = &entry.effect
                                {
                                    restored = ui.restore_refused(
                                        agent,
                                        text,
                                        in_reply_to.clone(),
                                        tags,
                                        images,
                                    );
                                }
                                if restored {
                                    ui.flash_for(
                                        format!("Not sent: {error}. Your message is back in the box"),
                                        super::FLASH_REFUSAL,
                                    );
                                } else {
                                    ui.flash_for(
                                        format!("Not sent: {error}"),
                                        super::FLASH_REFUSAL,
                                    );
                                }
                                entry.failed = Some(error);
                                entry.unconfirmed = unconfirmed;
                            }
                        }
                    }
                    if restored {
                        pending.retain(|entry| entry.token != token);
                    }
                    // st took it. The copy here gives way once the conversation shows it, which
                    // may already have happened; with no id to look for, at once.
                    pending.retain(|entry| {
                        if entry.token != token || entry.failed.is_some() {
                            return true;
                        }
                        // Already in the conversation when st answered: visible as sent now.
                        let waiting = entry.message_id.as_ref().is_some_and(|id| {
                            !timelines
                                .values()
                                .flat_map(|timeline| &timeline.items)
                                .any(|item| matches!(&item.body, TimelineBody::Message(message) if &message.message_id == id))
                        });
                        if !waiting {
                            super::slo::METRICS.observe("send_visible_ms", entry.since.elapsed());
                        }
                        waiting
                    });
                }
                Fetched::Preview(id, preview) => {
                    extras.previews.insert(id, preview);
                }
                Fetched::Body(id, from, title, content) => {
                    extras.bodies.insert(id, (from, title, content));
                }
                Fetched::Notice(notice) => ui.flash(notice),
                Fetched::AlertOutcome(id, accepted) => {
                    if !accepted {
                        answers.remove(&id);
                    } else if let Some(timing) = answers.get_mut(&id) {
                        timing.accepted = true;
                        if let Some(at) = timing.landed() {
                            landed_answers.push(at);
                            answers.remove(&id);
                        }
                    }
                }
                Fetched::Session(id, outcome) => match outcome {
                    Ok(session) => model.add_opened_session(*session),
                    Err(message) => {
                        if opens.remove(&id).is_some() {
                            super::slo::METRICS.add("open_cannot_load", 1);
                        }
                        ui.flash(format!("Could not open {id}: {message}"));
                        failed.insert(id, format!("{message} · open it again to retry"));
                    }
                },
                Fetched::SidebarSaved { write, outcome } => {
                    ui.resource_sidebar.saved(&write, outcome.is_ok());
                    match outcome {
                        Ok(()) => {}
                        // The reserved Sidebar is retired: never create it again.
                        Err((_, true)) if write.creating => {
                            ui.resource_sidebar.reserved_retired = true;
                            ui.flash(super::resource_sidebar::SIDEBAR_RETIRED);
                        }
                        Err((error, _)) => ui.flash(format!("Sidebar edit not done: {error}")),
                    }
                }
                Fetched::Observations { generation, outcome } => {
                    resources_reading = false;
                    if generation == resources_generation { match outcome {
                    Ok((rows, has_more)) => { ui.resource_sidebar.observations = rows; ui.resource_sidebar.observations_has_more = has_more; ui.resource_sidebar.observations_issue = None; }
                    Err(error) => ui.resource_sidebar.observations_issue = Some(error),
                }}},
                Fetched::Older {
                    target,
                    session_id,
                    page,
                    took,
                } => {
                    // A slow st is not asked to fill the first screen any further.
                    if slow_page(took) {
                        filled.insert(target.clone(), FILL_PAGES);
                    }
                    if let Some(timeline) = timelines.get_mut(&target) {
                        match page {
                            Ok(page) => timeline.older_page(
                                &session_id,
                                page.items,
                                page.has_more,
                                page.cursor,
                            ),
                            Err(reason) => timeline.older_failed(reason),
                        }
                    }
                }
                Fetched::Said(query, outcome) => {
                    ui.said = Some((query, outcome));
                    changed = true;
                }
                Fetched::Inventory { generation, result } => {
                    if generation != resources_generation || extras.inventory_supported != Some(false) { continue; }
                    legacy_pending.remove(&result.inventory());
                    if matches!(&result, InventoryRead::Usage(hours, _) if *hours != ui.usage_hours) { continue; }
                    let name = result.inventory().collection();
                    let error = match &result {
                        InventoryRead::Rows(_, outcome) => outcome.as_ref().err(),
                        InventoryRead::Terminals(outcome) => outcome.as_ref().err(),
                        InventoryRead::Clients(outcome) => outcome.as_ref().err(),
                        InventoryRead::Usage(_, outcome) => outcome.as_ref().err(),
                    };
                    note_legacy_read(&mut legacy_failing, &mut extras.window_errors, name, error, Instant::now());
                    match result {
                        // A failed read keeps the last good rows; the stale note says so if it lasts.
                        InventoryRead::Rows(inventory, outcome) => if let Ok(rows) = outcome {
                            match inventory {
                                Inventory::Machines => model.machines = rows,
                                Inventory::Devices => model.devices = rows,
                                Inventory::Sessions => model.sessions = rows,
                                _ => unreachable!(),
                            }
                        },
                        InventoryRead::Terminals(outcome) => if let Ok((rows, has_more)) = outcome {
                            ui.resource_sidebar.terminals = terminal_rows(rows);
                            ui.resource_sidebar.terminals_has_more = has_more;
                        },
                        InventoryRead::Clients(outcome) => match outcome {
                            Ok(clients) => model.clients = Some(Ok(clients)),
                            // The last good list stays; the stale note says so if it keeps failing.
                            Err(_) if matches!(model.clients, Some(Ok(_))) => {}
                            Err(error) => model.clients = Some(Err(error)),
                        },
                        InventoryRead::Usage(hours, outcome) if hours == ui.usage_hours => match outcome {
                            Ok(period) => model.usage = Some(Ok(period)),
                            Err(_) if matches!(model.usage, Some(Ok(_))) => {}
                            Err(error) => model.usage = Some(Err(error)),
                        },
                        InventoryRead::Usage(_, _) => {},
                    }
                }
                Fetched::GlassSaved { id, key, outcome } => ui.glass_saved(&id, &key, outcome),
                Fetched::TerminalStarted(id) => ui.terminal_started(id),
                Fetched::Native { agent, direct } => {
                    attach_note = None;
                    let waited = attach_started.take().map(|at| at.elapsed());
                    if let Some(waited) = waited.filter(|waited| *waited >= Duration::from_secs(3))
                    {
                        ui.flash(format!(
                            "Attached after {}s: st was slow to answer",
                            waited.as_secs()
                        ));
                    }
                    let (rows, columns) = ui.terminal_size.get();
                    let route = direct.route.clone();
                    if let Some(view) = ui
                        .terminal_view_mut(&agent)
                        .filter(|view| view.native.is_none())
                    {
                        // The route stays in the header, so the person can always see it.
                        view.name = format!("{} · {route}", view.name);
                        view.title = view.name.clone();
                        view.native = Some(super::pty::NativeTerminal::spawn(
                            direct.stream,
                            &direct.name,
                            direct.incarnation,
                            rows,
                            columns,
                        ));
                        view.stale = None;
                    }
                    ui.flash(format!("Attached · {route}"));
                }
                Fetched::Reattached { agent, outcome } => {
                    if let Some(native) = ui
                        .terminal_view(&agent)
                        .and_then(|view| view.native.as_ref())
                    {
                        match outcome {
                            Ok(direct) => native.reconnect(direct.stream, &direct.name),
                            Err(reason) if reason.contains("restarted") => native.give_up(reason),
                            // Still unreachable: the next try waits longer.
                            Err(reason) => native.retry_failed(reason),
                        }
                    }
                }
                Fetched::AttachWaiting { agent, reason } => {
                    if ui.terminal.as_ref().is_some_and(|view| view.agent == agent) {
                        attach_note = Some(reason);
                    }
                }
                Fetched::NativeFailed {
                    agent,
                    runtime_ids,
                    reason,
                } => {
                    attach_note = None;
                    if terminal_opens.remove(&agent).is_some() {
                        super::slo::METRICS.add("terminal_open_errors", 1);
                    }
                    attach_started = None;
                    if agent.starts_with("terminal/") {
                        // A shell has no other view to fall back to.
                        if let Some(view) = ui.terminal_view_mut(&agent) {
                            view.ended = Some(format!("could not attach: {reason}"));
                        }
                    } else if ui.terminal_view(&agent).is_some() {
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
        let showing_resources = ui.glasses.as_ref().is_some_and(|g| g.sidebar.shown);
        let focused_resource = match ui.focused_pane() { Some(super::pane::Pane::Resource(id)) => Some(id), _ => None };
        if tab != shown_tab || showing_resources && !resources_shown || focused_resource.is_some() && focused_resource != resource_pane { resources_stale = true; }
        resource_pane = focused_resource.clone();
        if ui.glasses.is_some() && extras.live && resources_stale && !resources_reading {
            resources_stale = false; resources_reading = true;
            let reader = client.clone(); let tx = fetched_tx.clone(); let generation = resources_generation;
            runtime.spawn(async move {
                let outcome = super::observed_resources::read(&reader, focused_resource.as_deref()).await.map_err(|error| format!("Not available on this connection: {}", error.plain()));
                let _ = tx.send(Fetched::Observations { generation, outcome });
            });
        }
        resources_shown = showing_resources;
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
        }
        let visible = ui.inventories_wanted();
        if let Some(hours) = visible.usage_hours && usage_wanted_hours != Some(hours) {
            usage_wanted_hours = Some(hours); model.usage = None; changed = true;
        }
        if extras.inventory_supported == Some(true) {
            let held = inventory_grace.followed(visible, Instant::now());
            if inventory_sent != Some(held) {
                inventory_sent = Some(held);
                let _ = commands.send(Command::Inventories(held));
            }
        } else if extras.inventory_supported == Some(false) && extras.live {
            let wanted = visible.windows().into_iter().collect::<BTreeSet<_>>();
            for inventory in wanted.difference(&legacy_requested).copied() {
                if !legacy_pending.insert(inventory) { continue; }
                let client = client.clone(); let tx = fetched_tx.clone(); let generation = resources_generation;
                runtime.spawn(async move { let result = legacy_inventory(&client, inventory).await; let _ = tx.send(Fetched::Inventory { generation, result }); });
            }
            legacy_requested = wanted;
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
        // Every conversation on screen rides the feed's socket (the focused one first): st
        // pushes each change, so nothing here reads one again on a timer.
        let wanted = ui.live_conversations();
        for target in &wanted {
            if !conversing.contains(target) {
                super::slo::METRICS.add("conversation_opens", 1);
                opens.insert(target.clone(), Instant::now());
            }
        }
        opens.retain(|target, _| wanted.contains(target));
        if let Some(trace) = &mut open_timing {
            trace.intend(&wanted);
        }
        retry_failed_opens(&mut sessions_requested, &failed, &wanted);
        for id in &wanted {
            if id.starts_with("session/")
                && !model.opened_sessions.contains_key(id)
                && sessions_requested.insert(id.clone())
            {
                // Opened again after a failure: loading, not the old error, until st answers.
                failed.remove(id);
                let client = client.clone();
                let tx = fetched_tx.clone();
                let id = id.clone();
                runtime.spawn(async move {
                    let outcome = Model::read_session(&client, &id)
                        .await
                        .map(Box::new)
                        .map_err(|error| error.to_string());
                    let _ = tx.send(Fetched::Session(id, outcome));
                });
            }
        }
        if wanted != conversing {
            let sent = commands.send(Command::Converse {
                targets: wanted.clone(),
            });
            if let Some(trace) = &mut open_timing {
                trace.enqueue(&wanted, sent.is_ok());
            }
            conversing = wanted;
            changed = true;
        }
        changed |= mark_unanswered(&mut pending);
        for target in moved_sessions(&conversing, &model, &timelines, &mut resubscribed) {
            let _ = commands.send(Command::Resubscribe { target });
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
            if let Effect::EditArrangement(write) = &effect {
                let write = write.clone();
                if !extras.live {
                    ui.resource_sidebar.saved(&write, false);
                    ui.flash("Offline · reconnect before editing the sidebar.");
                } else {
                    let client = client.clone(); let tx = fetched_tx.clone(); let person = person.clone(); let model = model.clone();
                    runtime.spawn(async move {
                        let outcome = perform_steadily(&client, &person, &model, effect, None).await.map(|_| ()).map_err(|error| {
                            let retired = error.chain().any(|cause| matches!(
                                cause.downcast_ref::<st3_client::ClientError>(),
                                Some(st3_client::ClientError::Api(st3_client::ErrorCode::ArrangementRetired, ..))
                            ));
                            (plain(&error), retired)
                        });
                        let _ = tx.send(Fetched::SidebarSaved { write, outcome });
                    });
                }
                continue;
            }
            if !extras.live && !matches!(effect, Effect::CloseTerminal) {
                if let Effect::LoadContent(key) = effect {
                    ui.content.finish(key, Err("Offline · reconnect, then load again".into()));
                }
                ui.flash(if extras.degraded.is_some() {
                    "Reconnecting to st · try again in a moment; nothing was queued"
                } else {
                    "Offline · reconnect before acting; nothing was queued"
                });
                continue;
            }
            if let Effect::Attention { id, .. } = &effect {
                if answers.len() < 256 || answers.contains_key(id) {
                    answers.insert(id.clone(), AnswerTiming::new(Instant::now()));
                } else {
                    super::slo::METRICS.add("measurement_overflow", 1);
                }
            }
            ui.note_acted(&effect);
            match effect {
                Effect::LoadContent(key) => {
                    content_reads.enqueue(key);
                }
                Effect::RetryConversation { target } => {
                    retry_conversation_open(
                        target,
                        &mut failed,
                        &mut sessions_requested,
                        &commands,
                    );
                    changed = true;
                }
                Effect::OpenTerminal { agent } => {
                    terminal_opens.insert(agent.clone(), Instant::now());
                    // The PTY session's own bytes, through st's raw stream to whichever host owns
                    // it; st's screen view (the feed follows it on its socket) only when st cannot
                    // give a direct stream.
                    let found = model
                        .agents()
                        .find(|candidate| candidate.header.id == agent);
                    let runtime_ids = found
                        .map(|candidate| candidate.runtime_ids.clone())
                        .unwrap_or_default();
                    // What st already said of the agent: its terminal is named after it, and its
                    // incarnation is the one running, so the attach needs no runtime read first.
                    let known = found.and_then(|candidate| {
                        let name = candidate
                            .runtime_ids
                            .first()?
                            .trim_start_matches("runtime/");
                        candidate.incarnation_id.clone().map(|incarnation| {
                            (name.to_owned(), format!("terminal/{agent}"), incarnation)
                        })
                    });
                    let name = found.map(crate::agent_label).unwrap_or_else(|| {
                        if agent.starts_with("terminal/") {
                            "shell".into()
                        } else {
                            agent.clone()
                        }
                    });
                    let owner = terminal_owner(&model, &agent, found, &runtime_ids);
                    attached = None;
                    terminal_runtimes = Some(runtime_ids.clone());
                    terminal_runtimes_by_agent.insert(agent.clone(), runtime_ids.clone());
                    match &owner {
                        Some(owner) => terminal_owners.insert(agent.clone(), owner.clone()),
                        None => terminal_owners.remove(&agent),
                    };
                    attach_started = Some(Instant::now());
                    attach_note = None;
                    let _ = commands.send(Command::Unfollow);
                    {
                        let client = client.clone();
                        let tx = fetched_tx.clone();
                        let agent = agent.clone();
                        let fallback = ui.terminal_fallback;
                        runtime.spawn(async move {
                            // A shell has no other view to fall back to, and one started a moment ago
                            // can be refused while st settles which host runs it: wait, say why, retry.
                            let delays: &[Duration] = if agent.starts_with("terminal/") {
                                &ATTACH_RETRY_DELAYS
                            } else {
                                &[]
                            };
                            let attached = retry_attach(
                                delays,
                                |reason| {
                                    let _ = tx.send(Fetched::AttachWaiting {
                                        agent: agent.clone(),
                                        reason: reason.to_owned(),
                                    });
                                },
                                || {
                                    attach_known_then_direct(
                                        &client,
                                        &agent,
                                        known.clone(),
                                        &runtime_ids,
                                        owner.as_deref(),
                                        fallback,
                                    )
                                },
                            )
                            .await;
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
                        ui.park_for(&agent);
                        ui.terminal_selection_mode = false;
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
        // Scrolling to the top of a conversation asks for the page before it; one at a time.
        // A first load that shows little (a page that is mostly tool calls) reads on by itself,
        // a few pages at most, until about a screenful of rows is there.
        let mut wanted = ui.take_older_wanted();
        let mut names = None;
        // One page is asked for at a time across every conversation, so opening several at
        // once is not a burst of reads on a daemon that may already be busy.
        let reading = timelines.values().any(|timeline| timeline.older.loading);
        for (target, timeline) in &timelines {
            if reading || !wanted.is_empty() {
                break;
            }
            let pages = filled.get(target).copied().unwrap_or(0);
            if !extras.live || !wants_fill(timeline, pages) {
                continue;
            }
            let names = names.get_or_insert_with(|| adapt::names(&model, &person));
            let entries = adapt::conversation(&timeline.items, names);
            if st3_conversation_ui::display_rows(&entries) < FILL_ROWS {
                *filled.entry(target.clone()).or_default() += 1;
                wanted.insert(target.clone());
            }
        }
        for target in wanted {
            let Some(timeline) = timelines.get_mut(&target) else {
                continue;
            };
            let Some(session_id) = timeline.session_id.clone() else {
                continue;
            };
            if !extras.live || timeline.older.loading || !timeline.more_before() {
                continue;
            }
            timeline.older.loading = true;
            changed = true;
            let cursor = timeline
                .older
                .cursor
                .as_ref()
                .filter(|(_, read)| read.elapsed() < OLDER_CURSOR_LIFE)
                .map(|(cursor, _)| cursor.clone());
            let oldest = timeline
                .items
                .first()
                .map(|entry| (entry.timestamp.clone(), entry.sequence));
            let client = client.clone();
            let tx = fetched_tx.clone();
            runtime.spawn(async move {
                let started = Instant::now();
                let page = older_page(&client, &session_id, cursor, oldest).await;
                let _ = tx.send(Fetched::Older {
                    target,
                    session_id,
                    page,
                    took: started.elapsed(),
                });
            });
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
                    retry.since = Instant::now();
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
                    let images = match &effect {
                        Effect::Send { images, .. } => images.len(),
                        _ => 0,
                    };
                    let shown = match (text.is_empty(), images) {
                        (_, 0) => text.clone(),
                        (true, count) => {
                            format!("▣ {count} image{}", if count == 1 { "" } else { "s" })
                        }
                        (false, count) => {
                            format!(
                                "{text}\n▣ {count} image{}",
                                if count == 1 { "" } else { "s" }
                            )
                        }
                    };
                    pending.push(Pending {
                        token: token.clone(),
                        agent: agent.clone(),
                        text: shown,
                        at: chrono::Local::now().format("%H:%M").to_string(),
                        message_id: None,
                        failed: None,
                        unconfirmed: false,
                        effect: effect.clone(),
                        sent: sent.clone(),
                        echoed: false,
                        since: Instant::now(),
                    });
                    changed = true;
                    (effect, Some(token), Some(sent))
                }
                other => (other, None, None),
            };
            let alert = match &effect {
                Effect::Attention { id, .. } => Some(id.clone()),
                _ => None,
            };
            let shell = matches!(effect, Effect::CreateTerminal { .. });
            let client = client.clone();
            let tx = fetched_tx.clone();
            let person = person.clone();
            let model = model.clone();
            let sends_message = token.is_some();
            runtime.spawn(async move {
                let outcome =
                    perform_steadily(&client, &person, &model, effect, sent.as_deref()).await;
                if let Some(id) = alert {
                    let _ = tx.send(Fetched::AlertOutcome(id, outcome.is_ok()));
                }
                if let Some(token) = token {
                    let _ = tx.send(Fetched::Sent(
                        token,
                        outcome.as_ref().map(|(_, id)| id.clone()).map_err(|error| {
                            // A transport failure left st's answer unknown; anything else is
                            // st saying no, or never reaching it.
                            let unconfirmed = error
                                .downcast_ref::<ClientError>()
                                .is_some_and(ClientError::outcome_unknown);
                            (plain(error), unconfirmed)
                        }),
                    ));
                }
                if shell && let Ok((_, Some(terminal))) = &outcome {
                    let _ = tx.send(Fetched::TerminalStarted(terminal.clone()));
                }
                match outcome {
                    Ok((notice, _)) => {
                        let _ = tx.send(Fetched::Notice(notice));
                    }
                    // A message's refusal is said, and its text put back, where its answer is
                    // handled (`Fetched::Sent`).
                    Err(_) if sends_message => {}
                    Err(error) => {
                        let _ = tx.send(Fetched::Notice(format!("Not done: {}", plain(&error))));
                    }
                }
            });
        }

        // The grace for a restart of st ends on its own clock: draw the link as offline then.
        match extras.offline_since {
            Some(since) if !offline_drawn && since.elapsed() >= adapt::offline_grace() => {
                offline_drawn = true;
                changed = true;
            }
            None => offline_drawn = false,
            Some(_) => {}
        }
        if changed {
            ui.content.index(&timelines);
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
            // An ask's images, from the attention just adapted (not the previous world's).
            if let Load::Ready(items) = &ui.world.attention {
                ui.content.index_requests(
                    items
                        .iter()
                        .flat_map(|item| match &item.kind {
                            super::view::AttentionKind::Request { images, .. } => images.clone(),
                            _ => Vec::new(),
                        })
                        .collect(),
                );
            }
            focus_initial_subject(&mut ui, &mut initial_subject, agents_live, glasses_ready);
            changed = false;
            if last_cache_save.elapsed() >= Duration::from_secs(60) {
                save_cache(cache_path.as_deref(), &person, &model);
                last_cache_save = Instant::now();
            }
        }
        // The missions window is large, so it is followed only while a mission list or card is on
        // screen, and kept for a while after so flipping between tabs does not fetch it each time.
        let wanted_now = ui.missions_wanted() || open_requests.wants_missions();
        if wanted_now {
            missions_until = Some(Instant::now() + MISSIONS_GRACE);
        }
        let wanted = wanted_now || missions_until.is_some_and(|until| Instant::now() < until);
        if wanted != missions_sent {
            missions_sent = wanted;
            let _ = commands.send(Command::Missions { follow: wanted });
        }
        answer_open_requests(&mut ui, &mut open_requests, agents_live && glasses_ready);
        ui.step_voice();
        ui.step_terminal_hold();
        ui.step_default_view();
        ui.step_terminal_tabs();
        terminal_opens.retain(|target, _| ui.terminal_view(target).is_some());
        ui.step_drafts();
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        let mut links = Vec::new();
        let draw_started = Instant::now();
        terminal.draw(|frame| {
            ui.render(frame);
            if hyperlinks {
                links = ui.link_cells(frame.buffer_mut());
            }
        })?;
        // A message just sent is on screen from this draw on: the time from pressing send to it.
        for entry in pending.iter_mut().filter(|entry| !entry.echoed) {
            entry.echoed = true;
            super::slo::METRICS.observe("send_echo_ms", entry.since.elapsed());
        }
        // A slow draw, or a whole turn that took long before it waited for input, is written
        // down with what was on screen, so that "it was slow" can be explained afterwards.
        let (drew, turn) = (draw_started.elapsed(), turn_started.elapsed());
        if drew >= SLOW_DRAW || turn >= SLOW_TURN {
            super::lastrun::note_slow(
                &format!("draw {} ms, turn {} ms", drew.as_millis(), turn.as_millis()),
                &ui.slow_context(),
                &crate::version::short(crate::version::now()),
            );
        }
        if hyperlinks && !links.is_empty() {
            super::hyperlinks::write_links(terminal.backend_mut(), &links)?;
        }
        // Image encodings the draw asked for, built off the draw path; the frame after one
        // arrives draws it.
        if let Some(picker) = ui.picker.clone() {
            for (key, width, height, image) in ui.content.protocol_jobs() {
                if !encoding.insert((key.clone(), width, height)) {
                    continue;
                }
                let picker = picker.clone();
                let tx = fetched_tx.clone();
                runtime.spawn_blocking(move || {
                    if let Some(protocol) = super::content::encode(&picker, &image, width, height) {
                        let _ = tx.send(Fetched::Protocol(key, width, height, protocol));
                    }
                });
            }
        }
        ui.release_far_images();
        if extras.live
            && let Some(key) = ui.next_content_read(&mut content_reads)
        {
            let client = client.clone();
            let tx = fetched_tx.clone();
            runtime.spawn(async move {
                let result = super::content::fetch(&client, &key).await;
                let _ = tx.send(Fetched::Content(key, result));
            });
        }
        // The attached terminal's cursor shape (vim's bar while inserting), and the person's
        // own shape back once it is gone.
        let style = ui.cursor_style();
        if style != cursor_style {
            let _ = crossterm::execute!(
                std::io::stdout(),
                style.unwrap_or(crossterm::cursor::SetCursorStyle::DefaultUserShape)
            );
            cursor_style = style;
        }
        super::open_timing::present(
            &mut open_timing,
            || execute!(io::stdout(), EndSynchronizedUpdate),
            |target| ui.conversation_entry_visible(target),
        )?;
        ui.quality.live(extras.live);
        let top = u8::from(extras.live)
            | (u8::from(extras.offline.is_some()) << 1)
            | (u8::from(extras.degraded.is_some()) << 2);
        let placeholders = u8::from(matches!(ui.world.attention, Load::Loading))
            | (u8::from(matches!(ui.world.agents, Load::Loading)) << 1);
        let visible_quality = ui.quality_visible();
        ui.quality
            .presented(ui.world.settled, top, placeholders, visible_quality);
        opens.retain(|target, at| {
            if ui.conversation_entry_visible(target) {
                super::slo::METRICS.observe("conversation_open_ms", at.elapsed());
                false
            } else {
                true
            }
        });
        terminal_opens.retain(|target, at| {
            if ui.terminal_ready_visible(target) {
                super::slo::METRICS.observe("terminal_open_ms", at.elapsed());
                false
            } else {
                true
            }
        });
        for at in landed_answers.drain(..) {
            super::slo::METRICS.observe("alert_landed_ms", at.elapsed());
        }
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
                let outcome = match acknowledge_visible_message(&client, &person, &id).await {
                    Ok(()) => ReceiptOutcome::Done,
                    Err(error) => receipt_outcome(&error),
                };
                let _ = tx.send(Fetched::Read(id, outcome));
            });
        }
        // A stream that dropped while the program ran attaches again, to the same incarnation,
        // after a wait that grows with each try (`pty::retry_delay`).
        if extras.live
            && let Some((agent, expected)) = ui.reattach_due()
        {
            let client = client.clone();
            let tx = fetched_tx.clone();
            let runtime_ids = terminal_runtimes_by_agent
                .get(&agent)
                .cloned()
                .unwrap_or_default();
            let owner = terminal_owners.get(&agent).cloned();
            let fallback = ui.terminal_fallback;
            runtime.spawn(async move {
                let outcome = attach_direct(
                    &client,
                    &agent,
                    &runtime_ids,
                    Some(&expected),
                    owner.as_deref(),
                    fallback,
                )
                .await;
                let _ = tx.send(Fetched::Reattached { agent, outcome });
            });
        }
        ui.terminal_requests();
        // While an attached terminal's output flows, or voice listens, draw it as it comes.
        let flowing = ui.voice.is_some()
            || ui
                .terminal
                .iter()
                .chain(ui.parked.iter())
                .filter_map(|view| view.native.as_ref())
                .any(|native| native.flowing());
        super::hover::poll_input(
            Duration::from_millis(if flowing { 16 } else { 80 }),
            &stopping,
            |input| {
                cancel_initial_subject(&mut initial_subject, &input);
                match input {
                    Event::Key(key) if reconnect_key(&key, extras.live) => {
                        let _ = commands.send(Command::Reconnect);
                        true
                    }
                    input => ui.input_event(input),
                }
            },
        )?;
    }
    super::lastrun::note_exit(if ui.quit {
        "quit (Ctrl+Q)"
    } else if stopping.load(std::sync::atomic::Ordering::Relaxed) {
        "signal (INT, TERM or HUP)"
    } else {
        "terminal closed (stdin hung up)"
    });
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
    ui.flush_drafts();
    Ok(())
}

/// When a notice read is wanted after an Attention or Agents frame: never for a window's first
/// frame on a connection (the read made at connect covers it), otherwise once the minimum gap since
/// the last read has passed, and a read already wanted keeps its time.
fn sync_due_after_frame(first: bool, read_at: Option<Instant>, due: Option<Instant>, now: Instant) -> Option<Instant> {
    if first || due.is_some() {
        return due;
    }
    Some(read_at.map_or(now, |at| at + SYNC_MIN_GAP).max(now))
}

/// Read the notice off a one-row Attention page, and send what it said (or that it failed).
fn read_sync(runtime: &tokio::runtime::Runtime, client: &Client, tx: &std::sync::mpsc::Sender<Fetched>) {
    let client = client.clone();
    let tx = tx.clone();
    runtime.spawn(async move {
        let notice = client
            .attention_list(None, Some(1), false)
            .await
            .map(|found| found.value.sync)
            .map_err(|_| ());
        let _ = tx.send(Fetched::Sync(notice));
    });
}

/// A followed window as the model holds it: its rows, the snapshot they came from, and the
/// notice the page read last found. The stream's frames carry no notice of their own, so every
/// window the stream rebuilds takes the page's, or the one with the newest snapshot would drop it.
fn window_collection(
    items: Vec<Resource>,
    snapshot: st3_client::Snapshot,
    has_more: bool,
    sync: Option<&st3_client::SyncNotice>,
) -> Collection {
    Collection { items, snapshot: Some(snapshot), truncated: has_more, sync: sync.cloned() }
}

/// Put a fresh page notice on every collection the stream follows, so `Model::sync_notice` finds it
/// whichever of them holds the newest snapshot.
fn fill_notice(model: &mut Model, sync: Option<&st3_client::SyncNotice>) {
    for collection in [
        &mut model.now,
        &mut model.missions,
        &mut model.agents,
        &mut model.sessions,
        &mut model.machines,
        &mut model.devices,
    ] {
        collection.sync = sync.cloned();
    }
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
            (Some(timeline), _) => {
                let mut entries = adapt::conversation(&timeline.items, &names);
                if let Some(note) = history_note(timeline) {
                    entries.insert(0, note);
                }
                Load::Ready(entries)
            }
            (None, Some(error)) => {
                Load::Failed(format!("Could not load this conversation: {error}"))
            }
            (None, None) => Load::Loading,
        };
        out.insert(target.to_owned(), load);
    }
    out
}

/// A session that could not be opened is asked for again once the person has left it and opens
/// it again; while it stays on screen, the error stays, so a refusal is not asked in a loop.
fn retry_failed_opens(
    requested: &mut HashSet<String>,
    failed: &BTreeMap<String, String>,
    wanted: &[String],
) {
    requested.retain(|id| !failed.contains_key(id) || wanted.contains(id));
}

/// A person's explicit retry clears both the feed failure and the one-shot session read.
fn retry_conversation_open(
    target: String,
    failed: &mut BTreeMap<String, String>,
    requested: &mut HashSet<String>,
    commands: &tokio::sync::mpsc::UnboundedSender<Command>,
) {
    failed.remove(&target);
    requested.remove(&target);
    let _ = commands.send(Command::Resubscribe { target });
}

/// Entries per page read back: st's largest, so a long session takes few requests.
const OLDER_PAGE: usize = 200;

/// About this many rows, as the person sees them, are there once a conversation opens.
const FILL_ROWS: usize = 200;

/// The most earlier pages one conversation reads on its own to get there.
const FILL_PAGES: usize = 7;

/// A page st took this long to give is a sign it is busy: no more pages are read on their own.
/// Ctrl+R asks to reconnect while there is no live link. A bare `r` is a letter like any other:
/// it must reach the message box, link or no link (Nathan, 2026-10-14: a lowercase r could not be
/// typed while the link was stale).
fn reconnect_key(key: &crossterm::event::KeyEvent, live: bool) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};
    !live
        && key.code == KeyCode::Char('r')
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

const FILL_SLOW: Duration = Duration::from_millis(1500);

fn slow_page(took: Duration) -> bool {
    took > FILL_SLOW
}

/// Whether a conversation may read one more earlier page on its own: there is more before it,
/// no page is in flight or has just failed, and it has not used up its pages.
fn wants_fill(timeline: &st3_conversation_ui::Timeline, pages_read: usize) -> bool {
    !timeline.older.loading
        && timeline.older.failed.is_none()
        && timeline.more_before()
        && pages_read < FILL_PAGES
}

/// st keeps a page cursor for five minutes; one older than this starts again from the newest.
const OLDER_CURSOR_LIFE: Duration = Duration::from_secs(240);

struct OlderPage {
    items: Vec<st3_client::TimelineEntry>,
    has_more: bool,
    cursor: Option<String>,
}

/// The page of `session_id`'s timeline before `oldest`. A live `cursor` continues where the
/// last page ended. Without one (or once st has let it go), pages are read again from the
/// newest until one reaches past `oldest`, so nothing between is skipped.
async fn older_page(
    client: &Client,
    session_id: &str,
    cursor: Option<String>,
    oldest: Option<(String, u64)>,
) -> Result<OlderPage, String> {
    let read = |cursor: Option<String>| async move {
        client
            .timeline(session_id, cursor.as_deref(), Some(OLDER_PAGE))
            .await
            .map(|found| OlderPage {
                has_more: found.value.page.has_more,
                cursor: found.value.page.next_cursor,
                items: found.value.items,
            })
    };
    if cursor.is_some() {
        match read(cursor).await {
            Ok(page) => return Ok(page),
            Err(ClientError::Api(st3_client::ErrorCode::PageCursorExpired, _, _)) => {}
            Err(error) => return Err(error.plain()),
        }
    }
    let mut cursor = None;
    // Bounded: a session longer than this many pages stops loading with a reason.
    for _ in 0..50 {
        let page = read(cursor).await.map_err(|error| error.plain())?;
        let reaches = oldest.as_ref().is_none_or(|(at, sequence)| {
            page.items
                .first()
                .is_some_and(|first| (&first.timestamp, first.sequence) < (at, *sequence))
        });
        if reaches || !page.has_more || page.cursor.is_none() {
            return Ok(page);
        }
        cursor = page.cursor;
    }
    Err("this session is too long to read further back here; st conversations timeline reads it all".into())
}

/// Say of each message st has not answered for a while that it is unconfirmed, so the person
/// can send it again (safely: the same request) or clear it, rather than watch it wait forever.
fn mark_unanswered(pending: &mut [Pending]) -> bool {
    let mut marked = false;
    for entry in pending {
        if entry.message_id.is_none()
            && entry.failed.is_none()
            && entry.since.elapsed() >= UNANSWERED_AFTER
        {
            entry.failed = Some("still waiting after 30 s".into());
            entry.unconfirmed = true;
            marked = true;
        }
    }
    marked
}

/// The followed agents whose conversation shows a session they have since left: st resolves an
/// agent to its session when a subscription starts, so after a restart that subscription keeps
/// the old one (Nathan's message to a restarted seat sat at "sending…" because it landed in the
/// new session, which his stui was not showing). Each is named once per new session.
fn moved_sessions(
    conversing: &[String],
    model: &Model,
    timelines: &BTreeMap<String, st3_conversation_ui::Timeline>,
    resubscribed: &mut BTreeMap<String, String>,
) -> Vec<String> {
    let mut moved = Vec::new();
    for target in conversing {
        let Some(current) = model
            .agents()
            .find(|agent| &agent.header.id == target)
            .and_then(|agent| agent.current_session_id.clone())
        else {
            continue;
        };
        let shown = timelines
            .get(target)
            .and_then(|timeline| timeline.session_id.as_deref());
        if shown.is_some_and(|shown| shown != current)
            && resubscribed.get(target) != Some(&current)
        {
            resubscribed.insert(target.clone(), current);
            moved.push(target.clone());
        }
    }
    moved
}

/// The quiet line above a conversation's oldest entry: how to see more, that more is on its
/// way, why it could not come, or that this is where the session starts.
/// The entry above a conversation's oldest that says how far back it goes.
pub(crate) const HISTORY_NOTE: &str = "history";

fn history_note(timeline: &st3_conversation_ui::Timeline) -> Option<super::view::Entry> {
    use st3_conversation_ui::Body;
    let older = &timeline.older;
    let text = if older.loading {
        "Loading earlier entries…".to_owned()
    } else if let Some(reason) = &older.failed {
        format!("Could not load earlier entries: {reason} · scroll up to try again")
    } else if timeline.more_before() {
        "Scroll up for earlier entries".to_owned()
    } else if timeline.items.is_empty() {
        return None;
    } else {
        "Start of this session · earlier ones: st conversations sessions".to_owned()
    };
    Some(super::view::Entry {
        id: HISTORY_NOTE.into(),
        at: String::new(),
        body: Body::Event(text),
    })
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
            Err(error) if not_applied(&error, &effect) => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(2));
            }
            outcome => return outcome,
        }
    }
    perform(client, person, model, effect, sent).await
}

/// Whether st refused a request in a way that guarantees it applied nothing and a fresh try
/// may succeed. An attention action already moved to its source's current card once on a
/// stale fence (`crate::attention_action`); trying its old card again cannot help.
fn not_applied(error: &anyhow::Error, effect: &Effect) -> bool {
    let attention = matches!(effect, Effect::Attention { .. });
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<ClientError>())
        .any(|error| match error {
            // st said it queued nothing, so a fresh try with a fresh fence is safe.
            _ if error.applied() == Some("none") => true,
            ClientError::Api(st3_client::ErrorCode::StaleFence, ..) => !attention,
            ClientError::Api(st3_client::ErrorCode::RateLimited, ..)
            | ClientError::Unreachable(_) => true,
            _ => false,
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

/// One agent action, fenced to the seat's current declaration as st asks. A restart is a stop
/// and then a start, each on the declaration st selects at that moment.
async fn agent_control(client: &Client, agent: &str, control: AgentControl) -> Result<()> {
    let fence = |definition: st3_client::Envelope<st3_client::SubjectDefinition>| Fence {
        snapshot_id: definition.snapshot.id,
        runtime_desired_revision: Some(definition.value.desired_token),
        ..Fence::default()
    };
    let reason = Some("from stui's agent actions".to_owned());
    let steps: &[AgentControl] = match control {
        AgentControl::Restart => &[AgentControl::Retire, AgentControl::Start],
        _ => std::slice::from_ref(&control),
    };
    for step in steps {
        let fence = fence(client.subject_definition(agent, false).await?);
        let (id, idem) = crate::action_pair();
        match step {
            AgentControl::Retire | AgentControl::Restart => {
                client
                    .agent_stop(
                        id,
                        idem,
                        fence,
                        st3_client::AgentStopParameters {
                            agent: agent.to_owned(),
                            reason: reason.clone(),
                        },
                    )
                    .await?;
            }
            AgentControl::Start => {
                client
                    .agent_start(
                        id,
                        idem,
                        fence,
                        st3_client::AgentStartParameters {
                            agent: agent.to_owned(),
                        },
                    )
                    .await?;
            }
            AgentControl::Suspend => {
                client
                    .agent_suspend(
                        id,
                        idem,
                        fence,
                        st3_client::AgentSuspendParameters {
                            agent: agent.to_owned(),
                            reason: reason.clone(),
                        },
                    )
                    .await?;
            }
            AgentControl::Resume => {
                client
                    .agent_resume(
                        id,
                        idem,
                        fence,
                        st3_client::AgentResumeParameters {
                            agent: agent.to_owned(),
                            host: None,
                        },
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

/// Submit a register edit with one stable action identity; the stream remains authoritative.
async fn save_sidebar(client: &Client, write: &super::resource_sidebar::Write) -> Result<()> {
    let snapshot = client.capabilities().await?.snapshot;
    let reply = client.arrangement_edit(
        format!("action/sidebar-{}", write.key), format!("sidebar-{}", write.key),
        Fence { snapshot_id: snapshot.id, ..Default::default() },
        st3_client::ArrangementEditParameters { subject: write.subject.clone(), owner: write.owner.clone(), operations: write.operations.clone() },
    ).await?;
    anyhow::ensure!(matches!(reply.value.status, st3_client::ActionStatus::Completed), "Sidebar edit is not completed.");
    Ok(())
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
        Effect::SaveGlass(_) | Effect::Resend { .. } | Effect::RetryConversation { .. }
        | Effect::Forget { .. } | Effect::LoadContent(_) => {
            Ok((String::new(), None))
        }
        Effect::EditArrangement(write) => {
            save_sidebar(client, &write).await?;
            Ok(("Sidebar edit saved".into(), None))
        }
        Effect::AgentControl { agent, control } => {
            agent_control(client, &agent, control).await?;
            Ok((
                format!(
                    "{} {}: asked st",
                    control.verb(),
                    agent.trim_start_matches("agent/")
                ),
                None,
            ))
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
        Effect::Attention {
            id,
            action,
            reason,
            answer,
        } => {
            let seen = model.attention().find(|card| card.header.id == id);
            crate::attention_action(client, person, &id, seen, &action, reason, answer)
                .await
                .map(|notice| (notice, None))
        }
        Effect::LaunchRevise { id, feedback } => {
            let current = client.attention_get(&id).await?;
            let Resource::Attention(attention) = &current.value else {
                anyhow::bail!("This launch changed; look again");
            };
            let launch_id = attention.launch_id.as_deref().unwrap_or_else(|| {
                attention
                    .source_id
                    .strip_prefix("planning-session/")
                    .unwrap_or(&attention.source_id)
            });
            let launch = client.launches_get(launch_id).await?;
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
                Vec::new(),
                None,
                latest_snapshot(model),
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
                Vec::new(),
                sent,
                latest_snapshot(model),
            )
            .await?;
            Ok((
                "Sent; the reply will show here and in their conversation".into(),
                id,
            ))
        }
        Effect::OpenImage { image } => {
            // In the person's own viewer where this machine has one; saved where they keep
            // downloads (ST3_DOWNLOADS or their download directory) where it has none.
            let bytes = client.blob(&image.sha256, Some(&image.message)).await?;
            let dir = super::attach::dir()
                .ok_or_else(|| anyhow::anyhow!("No place to keep the image (HOME is not set)"))?;
            let kept = super::attach::received(&dir, &image, &bytes)?;
            if super::attach::show(&kept) {
                return Ok((format!("Opened {}", image.name.as_deref().unwrap_or("the image")), None));
            }
            let downloads = super::attach::downloads()
                .ok_or_else(|| anyhow::anyhow!("No place to save the image (HOME is not set)"))?;
            let path = super::attach::save(&downloads, &image, &bytes)?;
            Ok((
                format!("Saved to {} · ST3_DOWNLOADS chooses where", path.display()),
                None,
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
                "Launch created; the planner's proposal will appear in Now".into(),
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
        Effect::Send {
            agent,
            mut text,
            in_reply_to,
            tags,
            images,
        } => {
            let session = model
                .agents()
                .find(|candidate| candidate.header.id == agent)
                .and_then(|agent| agent.current_session_id.clone());
            // Each image goes to st first; the message then carries them by reference, and the
            // bytes reach whichever machine reads them (#1078).
            let mut attachments = Vec::new();
            for (index, path) in images.iter().enumerate() {
                let bytes = std::fs::read(path).map_err(|error| {
                    anyhow::anyhow!("could not read {}: {error}", path.display())
                })?;
                match client
                    .upload_blob(bytes, super::attach::media_type(path))
                    .await
                {
                    Ok(upload) => attachments.push(st3_client::AttachmentInput {
                        blob: upload.value.blob,
                        media_type: upload.value.media_type,
                        name: path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned()),
                    }),
                    // An st from before attachments: name the files, as stui did then.
                    Err(st3_client::ClientError::Api(
                        st3_client::ErrorCode::NotFound
                        | st3_client::ErrorCode::UnsupportedCapability,
                        ..,
                    )) => {
                        let rest = images[index..]
                            .iter()
                            .filter_map(|path| {
                                super::attach::from_path(&path.display().to_string())
                            })
                            .collect::<Vec<_>>();
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(&super::attach::mention(&rest));
                        break;
                    }
                    Err(error) => anyhow::bail!("the image was not sent: {}", error.plain()),
                }
            }
            let id = send_message(
                client,
                &agent,
                text,
                None,
                in_reply_to,
                session,
                tags,
                attachments,
                sent,
                latest_snapshot(model),
            )
            .await?;
            Ok(("Message sent".into(), id))
        }
    }
}

/// The newest snapshot id any window frame brought, by store position.
fn latest_snapshot(model: &Model) -> Option<String> {
    [&model.now, &model.missions, &model.agents]
        .into_iter()
        .filter_map(|collection| collection.snapshot.as_ref())
        .max_by_key(|snapshot| snapshot.store_index)
        .map(|snapshot| snapshot.id.clone())
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
    attachments: Vec<st3_client::AttachmentInput>,
    sent: Option<&Mutex<Option<Sent>>>,
    snapshot_hint: Option<String>,
) -> Result<Option<String>> {
    let parameters = MessageSendParameters {
        to: to.to_owned(),
        content,
        title,
        in_reply_to,
        session_id,
        tags,
        kind: st3_client::MessageKind::Wake,
        attachments,
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
                earlier.parameters,
            )
            .await
        {
            Ok(result) => return Ok(message_id(result)),
            // st checks a repeat before the fence: a stale fence means it never took it.
            Err(ClientError::Api(st3_client::ErrorCode::StaleFence, ..)) => {}
            Err(error) => return Err(error.into()),
        }
    }
    // A send only needs a current snapshot. Take a fresh one each time, and once more if the
    // graph moves between reading it and sending: a stale fence is not the person's problem.
    // A message carries no subject revisions: st asks only that the snapshot be this host's and not
    // ahead of its store, so the newest one a window frame already brought is as good as a fresh
    // read and saves a round trip. Only if st calls it stale is a fresh one read.
    let mut hint = snapshot_hint;
    let mut last = None;
    for _ in 0..2 {
        let snapshot = match hint.take() {
            Some(known) => known,
            None => client.capabilities().await?.snapshot.id,
        };
        let (id, idem) = crate::action_pair();
        if let Some(sent) = sent
            && let Ok(mut slot) = sent.lock()
        {
            *slot = Some(Sent {
                id: id.clone(),
                idempotency_key: idem.clone(),
                snapshot_id: snapshot.clone(),
                parameters: parameters.clone(),
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
            Err(error @ ClientError::Api(st3_client::ErrorCode::StaleFence, ..)) => {
                last = Some(error)
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(last
        .map(Into::into)
        .unwrap_or_else(|| anyhow::anyhow!("the graph kept changing; try again")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_polled_inventory_that_fails_briefly_says_nothing_and_one_that_keeps_failing_is_said() {
        let start = Instant::now();
        let mut failing = BTreeMap::new();
        let mut errors = BTreeMap::new();
        let why = "st did not answer".to_owned();
        let stale = |errors: &BTreeMap<String, String>| {
            let extras = Extras { window_errors: errors.clone(), ..Extras::default() };
            adapt::world(&Model::default(), "person/avery", &extras).stale
        };

        // Young failures leave no error and no stale line, and the rows say loading, not none.
        note_legacy_read(&mut failing, &mut errors, "machines", Some(&why), start);
        note_legacy_read(&mut failing, &mut errors, "machines", Some(&why), start + feed::REPORT_AFTER - Duration::from_millis(1));
        assert!(errors.is_empty());
        assert!(stale(&errors).is_empty());
        let extras = Extras { window_errors: errors.clone(), ..Extras::default() };
        assert!(matches!(adapt::world(&Model::default(), "person/avery", &extras).machines, Load::Loading));

        // One that has lasted is said, in the stale note.
        note_legacy_read(&mut failing, &mut errors, "machines", Some(&why), start + feed::REPORT_AFTER);
        assert_eq!(errors.get("machines"), Some(&why));
        assert_eq!(stale(&errors), ["machines: st did not answer"]);

        // A read that succeeds clears the error and the clock; the next failure starts over.
        note_legacy_read(&mut failing, &mut errors, "machines", None, start + feed::REPORT_AFTER * 2);
        assert!(errors.is_empty() && failing.is_empty());
        assert!(stale(&errors).is_empty());
        note_legacy_read(&mut failing, &mut errors, "machines", Some(&why), start + feed::REPORT_AFTER * 3);
        assert!(errors.is_empty());
    }

    #[test]
    fn a_shells_attach_waits_out_a_refusal_that_passes_and_says_why() {
        // Nathan, 2026-10-16: a new terminal ended at once with "could not attach: st cannot tell
        // yet which host runs this", and the shell was lost to him.
        let run = |refusals: Vec<&'static str>, delays: usize| {
            let mut refusals = refusals.into_iter();
            let mut said = Vec::new();
            let outcome = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap()
                .block_on(retry_attach(
                    &vec![Duration::ZERO; delays],
                    |reason| said.push(reason.to_owned()),
                    || {
                        let next = refusals.next();
                        async move { next.map_or(Ok(7), |reason| Err(reason.to_owned())) }
                    },
                ));
            (outcome, said)
        };
        let unsure = "st cannot tell yet which host runs this";
        // It works on the third try; each wait said why.
        let (outcome, said) = run(vec![unsure, "host/example cannot be reached right now"], 5);
        assert_eq!(outcome, Ok(7));
        assert_eq!(said, [unsure, "host/example cannot be reached right now"]);
        // It keeps refusing: the last refusal comes back once the waits are used up.
        let (outcome, said) = run(vec![unsure; 10], 3);
        assert_eq!(outcome, Err(unsure.to_owned()));
        assert_eq!(said.len(), 3);
        // A terminal that is gone, restarted or not allowed is not retried.
        for reason in ["it is gone: terminal/x", "not allowed: nope", direct::RESTARTED] {
            let (outcome, said) = run(vec![reason, unsure], 5);
            assert_eq!(outcome, Err(reason.to_owned()));
            assert!(said.is_empty(), "{reason} must not be retried");
        }
        // The retried reasons are the words st's own messages use.
        assert!(super::worth_retrying(&st3_client::plain_message(
            Some(&st3_client::ErrorCode::RuntimeAuthorityIndeterminate),
            ""
        )));
        assert!(!super::worth_retrying(&st3_client::plain_message(
            Some(&st3_client::ErrorCode::NotFound),
            "terminal/x"
        )));
        // A refusal that passes waits about a minute and a half before the tab gives up.
        assert!(super::ATTACH_RETRY_DELAYS.iter().sum::<Duration>() >= Duration::from_secs(90));
    }

    #[test]
    fn hidden_inventory_windows_leave_after_grace_and_period_changes_replace_one_slot() {
        let now = Instant::now();
        let mut grace = InventoryGrace::default();
        let visible = Inventories { clients: true, usage_hours: Some(24), ..Inventories::default() };
        assert_eq!(grace.followed(visible, now), visible);
        assert_eq!(grace.followed(Inventories::default(), now + Duration::from_secs(29)), visible);
        assert_eq!(grace.followed(Inventories::default(), now + MISSIONS_GRACE), Inventories::default());
        grace.followed(visible, now);
        let week = grace.followed(Inventories { usage_hours: Some(168), ..Inventories::default() }, now + Duration::from_secs(1));
        assert_eq!(week.windows(), vec![Inventory::Clients, Inventory::Usage(168)]);
        assert_eq!(grace.followed(Inventories::default(), now + MISSIONS_GRACE).windows(), vec![Inventory::Usage(168)]);
    }

    #[test]
    fn an_answer_landing_requires_action_confirmation_and_the_fresh_window_in_either_order() {
        let at = Instant::now();
        let mut window_first = AnswerTiming::new(at);
        window_first.gone = true;
        assert!(
            window_first.landed().is_none(),
            "a card closed by another device is not our successful answer"
        );
        window_first.accepted = true;
        assert_eq!(window_first.landed(), Some(at));
        let mut reply_first = AnswerTiming::new(at);
        reply_first.accepted = true;
        assert!(reply_first.landed().is_none());
        reply_first.gone = true;
        assert_eq!(reply_first.landed(), Some(at));
    }

    #[test]
    fn a_send_st_may_have_taken_is_unconfirmed_and_one_it_never_queued_is_tried_afresh() {
        let answer = |applied: &str, code: st3_client::ErrorCode| -> anyhow::Error {
            let envelope = st3_client::ErrorEnvelope {
                api_version: "st3.client.v0".into(),
                error_version: "st3.client.error.v0".into(),
                request_id: "request/test".into(),
                code: code.clone(),
                message: "the database has not confirmed the send".into(),
                retryable: false,
                retry_after_ms: None,
                details: std::collections::BTreeMap::from([(
                    "applied".to_owned(),
                    serde_json::json!(applied),
                )]),
            };
            ClientError::Api(code, envelope.message.clone(), Box::new(envelope)).into()
        };
        let send = Effect::Send {
            agent: "agent/x".into(),
            text: "hello".into(),
            in_reply_to: None,
            tags: Vec::new(),
            images: Vec::new(),
        };
        // `applied: "unknown"` (a code this build does not know decodes as Unknown): the exact
        // request may have been taken. It is not retried here, and it is shown as unconfirmed.
        let unknown = answer("unknown", st3_client::ErrorCode::Unknown);
        assert!(!not_applied(&unknown, &send));
        assert!(unknown.downcast_ref::<ClientError>().unwrap().outcome_unknown());
        assert_eq!(
            super::plain(&unknown),
            "st may have taken it and has not confirmed it yet; sending it again is safe"
        );
        // `applied: "none"` with a full queue (rate limited): nothing was queued, so try again.
        assert!(not_applied(&answer("none", st3_client::ErrorCode::RateLimited), &send));
        assert!(!answer("none", st3_client::ErrorCode::RateLimited)
            .downcast_ref::<ClientError>()
            .unwrap()
            .outcome_unknown());
    }

    /// st answers a send it could not confirm in time `message-send-unconfirmed`, `applied:
    /// "unknown"`: it may still complete. Nothing here sends it again by itself, and the person's
    /// resend repeats the exact request. st keys its receipt on the whole request but the fence,
    /// so a repeat that differs in anything else (here, the agent moved to a new session in
    /// between) would be refused as a conflict and the message could never be confirmed.
    #[tokio::test]
    async fn an_unconfirmed_send_waits_for_the_person_and_goes_again_unchanged() {
        use axum::{body::Bytes, extract::State, http::StatusCode, routing};
        #[derive(Default)]
        struct Daemon {
            posted: Vec<serde_json::Value>,
            receipts: BTreeMap<String, serde_json::Value>,
        }
        type Shared = Arc<Mutex<Daemon>>;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("daemon.sock");
        let capabilities: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../docs/st3/client-v0/fixtures/capabilities.json"
        ))
        .unwrap();
        let snapshot = capabilities["snapshot"].clone();
        // The first request under a key is taken but not confirmed; the same request again gets
        // its result; a different request under that key is a conflict, as st answers.
        let action = move |State(daemon): State<Shared>, body: Bytes| {
            let snapshot = snapshot.clone();
            async move {
                let action: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let mut daemon = daemon.lock().unwrap();
                daemon.posted.push(action.clone());
                let key = action["idempotency_key"].as_str().unwrap().to_owned();
                let mut digest = action.clone();
                digest.as_object_mut().unwrap().remove("fence");
                let refusal = |code: &str, retryable: bool, details: serde_json::Value| {
                    serde_json::json!({
                        "api_version": "st3.client.v0", "error_version": "st3.client.error.v0",
                        "request_id": "request/example", "code": code, "message": code,
                        "retryable": retryable, "details": details,
                    })
                };
                let (status, body) = match daemon.receipts.get(&key) {
                    Some(seen) if *seen != digest => (
                        StatusCode::CONFLICT,
                        refusal("idempotency-conflict", false, serde_json::json!({})),
                    ),
                    Some(_) => (
                        StatusCode::OK,
                        serde_json::json!({
                            "api_version": "st3.client.v0", "request_id": "request/example",
                            "snapshot": snapshot,
                            "value": {
                                "kind": "action-result", "action_id": action["id"],
                                "operation_id": "operation/example", "status": "completed",
                                "affected_ids": ["message/example-tally"],
                                "snapshot_id": snapshot["id"],
                            },
                        }),
                    ),
                    None => {
                        daemon.receipts.insert(key.clone(), digest);
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            refusal(
                                "message-send-unconfirmed",
                                false,
                                serde_json::json!({
                                    "applied": "unknown", "action_id": action["id"],
                                    "idempotency_key": key,
                                }),
                            ),
                        )
                    }
                };
                (status, axum::Json(body))
            }
        };
        let daemon = Shared::default();
        let router = axum::Router::new()
            .route(
                "/v1/client/capabilities",
                routing::get(move || std::future::ready(axum::Json(capabilities.clone()))),
            )
            .route("/v1/client/actions", routing::post(action))
            .with_state(daemon.clone());
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = Client::unix_as(&socket, "person/avery");
        let in_session = |session: &str| {
            let mut model = Model::default();
            model.agents.items.push(
                serde_json::from_value(serde_json::json!({
                    "kind": "agent", "id": "agent/example/harbor/keeper", "revision": "a",
                    "updated_at": "2026-10-03T08:28:00Z", "name": "Keeper", "state": "running",
                    "reachability": "reachable", "current_session_id": session,
                }))
                .unwrap(),
            );
            model
        };
        let send = Effect::Send {
            agent: "agent/example/harbor/keeper".into(),
            text: "Count the harbor crates".into(),
            in_reply_to: None,
            tags: vec!["tally".into()],
            images: Vec::new(),
        };
        // The live loop's own path: the same effect and the one `sent` slot for its pending copy.
        let sent = Arc::new(Mutex::new(None));
        let error = perform_steadily(
            &client,
            "person/avery",
            &in_session("session/first"),
            send.clone(),
            Some(&*sent),
        )
        .await
        .unwrap_err();
        // What `Fetched::Sent` marks the pending copy with: unconfirmed, so it stays and offers
        // a resend, not a plain failure.
        assert!(error
            .downcast_ref::<ClientError>()
            .is_some_and(ClientError::outcome_unknown));
        assert!(!not_applied(&error, &send));
        assert_eq!(
            super::plain(&error),
            "st may have taken it and has not confirmed it yet; sending it again is safe"
        );
        assert_eq!(daemon.lock().unwrap().posted.len(), 1, "not sent again by itself");
        // The agent moved to a new session before the person sent it again.
        let (_, message) = perform_steadily(
            &client,
            "person/avery",
            &in_session("session/second"),
            send,
            Some(&*sent),
        )
        .await
        .unwrap();
        assert_eq!(message.as_deref(), Some("message/example-tally"));
        let posted = std::mem::take(&mut daemon.lock().unwrap().posted);
        assert_eq!(posted.len(), 2);
        for field in ["id", "type", "idempotency_key", "parameters"] {
            assert_eq!(posted[1][field], posted[0][field], "{field} differs on the resend");
        }
        assert_eq!(posted[0]["parameters"]["session_id"], "session/first");
        server.abort();
    }

    #[test]
    fn a_slow_page_stops_the_first_screen_reading_on() {
        assert!(!super::slow_page(Duration::from_millis(400)));
        assert!(super::slow_page(Duration::from_secs(2)));
    }

    #[test]
    fn a_first_screen_reads_earlier_pages_only_while_it_may() {
        let mut timeline = st3_conversation_ui::Timeline {
            has_more: true,
            ..Default::default()
        };
        assert!(super::wants_fill(&timeline, 0));
        assert!(super::wants_fill(&timeline, super::FILL_PAGES - 1));
        assert!(!super::wants_fill(&timeline, super::FILL_PAGES));
        timeline.older.loading = true;
        assert!(!super::wants_fill(&timeline, 0));
        timeline.older.loading = false;
        timeline.older.failed = Some("st is slow".into());
        assert!(!super::wants_fill(&timeline, 0));
        timeline.older.failed = None;
        timeline.has_more = false;
        assert!(!super::wants_fill(&timeline, 0));
    }

    #[test]
    fn a_terminal_that_is_slow_to_connect_says_how_long_and_who_is_slow() {
        use super::connecting_text;
        use std::time::Duration;
        assert_eq!(connecting_text(Duration::from_millis(900)), "connecting");
        assert_eq!(connecting_text(Duration::from_secs(3)), "connecting 3s");
        assert_eq!(
            connecting_text(Duration::from_secs(12)),
            "connecting 12s, st is slow to answer"
        );
    }

    use super::*;

    fn peer(host: &str, missing: u64, diverged: bool) -> st3_client::SyncPeer {
        st3_client::SyncPeer {
            last_successful_exchange: None,
            last_failed_exchange: None,
            host_id: host.into(),
            peer_only_envelopes: missing,
            local_only_envelopes: 0,
            last_exchange_at: None,
            estimated_catch_up_seconds: Some(840),
            diverged_since: diverged.then(|| "2026-09-29T10:00:00Z".into()),
        }
    }

    fn snapshot(store_index: u64) -> st3_client::Snapshot {
        st3_client::Snapshot {
            id: format!("snapshot/{store_index}"),
            host_id: "host/hearth".into(),
            store_index,
            projection_version: "1".into(),
            created_at: "2026-10-16T11:00:00Z".into(),
        }
    }

    /// The world a running stui shows after the stream delivered Attention (older) and Agents
    /// (newest) windows, built by the same function the live loop uses.
    fn running_world(sync: Option<&st3_client::SyncNotice>) -> super::super::view::World {
        let mut model = Model::default();
        let now = window_collection(Vec::new(), snapshot(5), false, sync);
        let agents = window_collection(Vec::new(), snapshot(9), false, sync);
        model.now = now;
        model.agents = agents;
        let extras = super::super::adapt::Extras { live: true, ..Default::default() };
        super::super::adapt::world(&model, "person/avery", &extras)
    }

    #[test]
    fn a_running_stui_says_diverged_or_behind_from_the_notice_the_windows_carry_and_live_otherwise() {
        use super::super::view::Link;
        let diverged = st3_client::SyncNotice { state: "diverged".into(), peers: vec![peer("host/harbor", 5, true)] };
        let shown = running_world(Some(&diverged));
        assert!(matches!(shown.link, Link::Live));
        assert_eq!(shown.diverged, vec!["harbor".to_owned()]);
        assert!(shown.behind.is_empty());

        let behind = st3_client::SyncNotice { state: "catching-up".into(), peers: vec![peer("host/harbor", 26_820, false)] };
        let shown = running_world(Some(&behind));
        assert!(shown.diverged.is_empty());
        assert_eq!(shown.behind.len(), 1, "{:?}", shown.behind);
        assert!(shown.behind[0].starts_with("harbor has 26,820 envelopes"), "{:?}", shown.behind);

        // No notice, or one that names nothing missing: nothing is claimed, and the link is live.
        for sync in [None, Some(st3_client::SyncNotice { state: "catching-up".into(), peers: vec![peer("host/dock", 0, false)] })] {
            let shown = running_world(sync.as_ref());
            assert!(matches!(shown.link, Link::Live));
            assert!(shown.diverged.is_empty() && shown.behind.is_empty(), "{:?} {:?}", shown.diverged, shown.behind);
        }
    }

    #[test]
    fn a_notice_read_follows_a_moving_graph_at_most_once_a_minute_and_an_idle_screen_sends_none() {
        let now = Instant::now();
        // First frames are not changes: no read is wanted by them.
        assert_eq!(sync_due_after_frame(true, Some(now), None, now), None);
        // A later frame soon after a read waits out the minimum gap; a long-ago read is due now.
        assert_eq!(sync_due_after_frame(false, Some(now), None, now + Duration::from_secs(5)), Some(now + SYNC_MIN_GAP));
        let late = now + SYNC_MIN_GAP + Duration::from_secs(30);
        assert_eq!(sync_due_after_frame(false, Some(now), None, late), Some(late));
        // A read already wanted keeps its time however many frames follow; no read made yet is due now.
        let wanted = Some(now + Duration::from_secs(40));
        assert_eq!(sync_due_after_frame(false, Some(now), wanted, now + Duration::from_secs(10)), wanted);
        assert_eq!(sync_due_after_frame(false, None, None, now), Some(now));
    }

    #[test]
    fn the_page_read_fills_the_notice_and_a_rebuilt_window_keeps_it_until_the_next_read() {
        let diverged = st3_client::SyncNotice { state: "diverged".into(), peers: vec![peer("host/harbor", 5, true)] };
        // The stream's windows arrive with nothing known, the newest being Agents.
        let mut model = Model::default();
        let now = window_collection(Vec::new(), snapshot(5), false, None);
        let agents = window_collection(Vec::new(), snapshot(9), false, None);
        model.now = now;
        model.agents = agents;
        assert!(model.sync_notice().is_none());
        // The page read finds a diverged peer: it shows although Agents holds the newest snapshot.
        fill_notice(&mut model, Some(&diverged));
        assert_eq!(model.sync_notice(), Some(&diverged));
        // A later frame rebuilds Agents with the notice the loop carries, not with none.
        model.agents = window_collection(Vec::new(), snapshot(12), false, Some(&diverged));
        assert_eq!(model.sync_notice(), Some(&diverged));
        // The next read says level: the notice goes from every window.
        fill_notice(&mut model, None);
        assert!(model.sync_notice().is_none());
    }

    #[test]
    fn open_requests_wait_for_a_live_inventory_and_answer_each_seat_once() {
        use super::super::requests::{self, Answer, Inbox, Place};
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("requests");
        let mut world = super::super::demo::world();
        world.link = super::super::view::Link::Live;
        let known = world.agents.items()[1].id.clone();
        let mut ui = Ui::new(world);
        ui.glasses = Some(super::super::glass::Glasses::open(None, None));
        ui.show_focused();
        let mut inbox = Inbox::new(Some(dir.clone()));
        let early = requests::enqueue(&dir, &known, Place::Right, false, None).unwrap();
        assert!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o077 == 0);

        // Cached inventory is not a reason to open anything.
        answer_open_requests(&mut ui, &mut inbox, false);
        assert_eq!(ui.live_conversations().iter().filter(|id| **id == known).count(), 0);
        assert!(dir.join(format!("{early}.json")).exists());

        answer_open_requests(&mut ui, &mut inbox, true);
        assert_eq!(ui.focus(), (1, Some(known.clone())));
        assert!(!dir.join(format!("{early}.json")).exists());
        let answer = std::fs::read(dir.join(format!("{early}.answer"))).unwrap();
        assert_eq!(
            serde_json::from_slice::<Answer>(&answer).unwrap(),
            Answer::Opened { focused: true }
        );

        // An agent that is not in the inventory yet waits, and is not found once the grace ends.
        let later = requests::enqueue(&dir, "agent/garden/not-yet", Place::Tab, false, None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(350));
        answer_open_requests(&mut ui, &mut inbox, true);
        assert!(dir.join(format!("{later}.json")).exists(), "still waiting for st to know it");
        assert_eq!(inbox.waiting().count(), 1);
        inbox.waiting_since_for_test(&later, requests::GRACE);
        answer_open_requests(&mut ui, &mut inbox, true);
        let answer = std::fs::read(dir.join(format!("{later}.answer"))).unwrap();
        assert_eq!(serde_json::from_slice::<Answer>(&answer).unwrap(), Answer::NotFound);
        assert_eq!(inbox.waiting().count(), 0);
    }

    #[test]
    fn a_request_the_layout_cannot_show_without_moving_focus_is_not_called_opened() {
        use super::super::pane::Pane;
        use super::super::requests::{Answer, Place};
        let mut world = super::super::demo::world();
        world.link = super::super::view::Link::Live;
        let known = world.agents.items()[1].id.clone();
        let mut ui = Ui::new(world);
        ui.glasses = None;
        let pane = || Pane::Agent(Some(known.clone()));
        // The classic layout has no tabs or splits: only the list can show it, which moves focus.
        let kept = ui.open_requested(pane(), Place::Tab, true, None);
        assert_eq!(kept, Answer::NotShown);
        let moved = ui.open_requested(pane(), Place::Tab, false, None);
        assert_eq!(moved, Answer::Opened { focused: true });
        // A name another process wrote cannot put control characters in the flash.
        ui.glasses = Some(super::super::glass::Glasses::open(None, None));
        ui.show_focused();
        ui.open_requested(pane(), Place::Right, false, Some("agent/x/bad\x1b[2Jname\n"));
        let (shown, _) = ui.flash.clone().unwrap();
        assert!(!shown.chars().any(|c| c.is_control()), "{shown:?}");
        assert!(shown.contains("bad[2Jname"), "{shown:?}");
    }

    #[test]
    fn only_ctrl_r_reconnects_and_never_a_letter() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let ctrl_r = key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let plain_r = key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(reconnect_key(&ctrl_r, false), "Ctrl+R retries a link that is down");
        assert!(!reconnect_key(&ctrl_r, true), "Ctrl+R is left to the conversation when live");
        assert!(!reconnect_key(&plain_r, false), "a lowercase r is typed, never taken");
        assert!(!reconnect_key(&key(KeyCode::Char('R'), KeyModifiers::SHIFT), false));
    }

    #[test]
    fn initial_subject_waits_for_live_inventory_and_opens_only_once() {
        for spaces in [false, true] {
            let mut world = super::super::demo::world();
            let mut assistant = world.agents.items()[0].clone();
            assistant.id = "agent/st/assistant".into();
            assistant.name = "Smalltalk Assistant".into();
            let other = world.agents.items()[0].id.clone();
            let mut ui = Ui::new(world.clone());
            if spaces {
                ui.glasses = Some(super::super::glass::Glasses::open(None, None));
                ui.show_focused();
                ui.open_home();
            }
            let before = ui.focus();
            let mut requested = Some(assistant.id.clone());
            focus_initial_subject(&mut ui, &mut requested, false, true);
            assert_eq!(
                ui.focus(), before,
                "waiting inventory must not open another agent"
            );
            assert!(requested.is_some());
            world.agents = Load::Ready(vec![world.agents.items()[0].clone(), assistant.clone()]);
            ui.set_world(world.clone());
            let before = ui.focus();
            focus_initial_subject(&mut ui, &mut requested, false, true);
            assert_eq!(
                ui.focus(), before,
                "cached inventory must not consume the request"
            );
            focus_initial_subject(&mut ui, &mut requested, true, true);
            assert_eq!(ui.focus(), (1, Some(assistant.id.clone())));
            assert_eq!(ui.live_conversations()[0], assistant.id);
            assert!(!ui.home_open());
            assert!(requested.is_none());
            ui.open(&other);
            ui.set_world(world);
            focus_initial_subject(&mut ui, &mut requested, true, true);
            assert_eq!(
                ui.focus(), (1, Some(other)),
                "later updates preserve navigation"
            );
        }
    }

    #[test]
    fn initial_subject_is_cancelled_by_navigation_or_a_missing_live_agent() {
        let mut ui = Ui::new(super::super::demo::world());
        let mut missing = Some("agent/st/assistant".to_owned());
        let before = ui.focus();
        focus_initial_subject(&mut ui, &mut missing, true, true);
        assert!(missing.is_none());
        assert_eq!(ui.focus(), before);
        let inputs = [
            Event::Key(crossterm::event::KeyCode::Down.into()),
            Event::Paste("hello".into()),
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 5, row: 5, modifiers: crossterm::event::KeyModifiers::NONE,
            }),
        ];
        for input in inputs {
            let mut subject = Some(ui.world.agents.items()[0].id.clone());
            cancel_initial_subject(&mut subject, &Event::Resize(120, 32));
            assert!(subject.is_some());
            cancel_initial_subject(&mut subject, &input);
            focus_initial_subject(&mut ui, &mut subject, true, true);
            assert_eq!(ui.focus(), before);
            assert!(subject.is_none());
        }
    }

    #[test]
    fn no_initial_subject_preserves_the_restored_space() {
        let mut ui = Ui::new(super::super::demo::world());
        ui.glasses = Some(super::super::glass::Glasses::open(None, None));
        let agent = ui.world.agents.items()[0].id.clone();
        ui.open(&agent);
        ui.open_home();
        let before = ui.focus();
        focus_initial_subject(&mut ui, &mut None, true, true);
        assert_eq!(ui.focus(), before);
        assert!(ui.home_open());
    }

    fn refusal(code: st3_client::ErrorCode, retryable: bool) -> anyhow::Error {
        anyhow::Error::new(st3_client::ClientError::Api(
            code.clone(),
            "refused".into(),
            Box::new(st3_client::ErrorEnvelope {
                api_version: "st3.client.v0".into(),
                error_version: "st3.client.error.v0".into(),
                request_id: "request/test".into(),
                code,
                message: "refused".into(),
                retryable,
                retry_after_ms: None,
                details: Default::default(),
            }),
        ))
    }

    #[test]
    fn a_read_receipt_st_refuses_for_good_is_dropped_and_never_asked_again() {
        // Read-only identity: st says forbidden and not retryable.
        assert_eq!(
            receipt_outcome(&refusal(st3_client::ErrorCode::Forbidden, false)),
            ReceiptOutcome::Refused
        );
        // A stale fence is answered by a fresh one, and a retryable refusal or a lost connection is retried.
        assert_eq!(
            receipt_outcome(&refusal(st3_client::ErrorCode::StaleFence, false)),
            ReceiptOutcome::Retry
        );
        assert_eq!(
            receipt_outcome(&refusal(st3_client::ErrorCode::Forbidden, true)),
            ReceiptOutcome::Retry
        );
        assert_eq!(
            receipt_outcome(&anyhow::anyhow!("the connection broke")),
            ReceiptOutcome::Retry
        );
        let mut receipts = ReadReceipts::default();
        let now = Instant::now();
        receipts.displayed("message/a".into(), now);
        assert_eq!(receipts.next(now).as_deref(), Some("message/a"));
        receipts.completed("message/a".into(), ReceiptOutcome::Refused, now);
        // Shown again, asked for nothing more.
        receipts.displayed("message/a".into(), now);
        assert!(receipts.next(now + Duration::from_secs(3600)).is_none());
    }

    #[test]
    fn a_read_receipt_that_keeps_failing_waits_longer_each_time_up_to_a_minute() {
        let mut receipts = ReadReceipts::default();
        let start = Instant::now();
        receipts.displayed("message/b".into(), start);
        let mut waits = Vec::new();
        for _ in 0..8 {
            let at = receipts.pending["message/b"].unwrap();
            assert_eq!(receipts.next(at).as_deref(), Some("message/b"));
            receipts.completed("message/b".into(), ReceiptOutcome::Retry, start);
            waits.push(receipts.pending["message/b"].unwrap().duration_since(start).as_secs());
        }
        assert_eq!(waits, [2, 4, 8, 16, 32, 60, 60, 60]);
        // One success clears it.
        let at = receipts.pending["message/b"].unwrap();
        assert!(receipts.next(at).is_some());
        receipts.completed("message/b".into(), ReceiptOutcome::Done, start);
        assert!(receipts.next(start + Duration::from_secs(3600)).is_none());
    }

    #[tokio::test]
    async fn a_send_uses_the_snapshot_a_window_brought_and_reads_a_fresh_one_only_when_st_calls_it_stale() {
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("daemon.sock");
        let store = Arc::new(st3::store::Store::open(&root.path().join("graph.db"), "ui-hint").unwrap());
        let state = st3::api::AppState {
            owning_person: None,
            store: store.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
            event_notify: tokio::sync::watch::channel(0_u64).0,
            node: "ui-hint".into(),
            state_dir: root.path().into(),
            pty_root: root.path().join("pty"),
            pty_binary: "pty".into(),
            fleet_id: None,
            configured_peers: vec![],
            client_relay: None,
            native_session_home: None,
            planner_default: Default::default(),
        };
        let path = socket.clone();
        let server = tokio::spawn(async move {
            st3::api::serve_unix(&path, st3::api::router(state)).await.unwrap();
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let client = Client::unix_as(&socket, "person/avery");
        // The snapshot an earlier window frame brought: this host's, behind the store now.
        let earlier = client.capabilities().await.unwrap().snapshot.id;
        let first = send_message(&client, "agent/example/worker", "one".into(), None, None, None, vec![], vec![], None, Some(earlier.clone()))
            .await
            .unwrap();
        assert!(first.is_some(), "a snapshot from before the store moved is accepted");
        let count = |store: &st3::store::Store, text: &str| {
            store.message(first.as_deref().unwrap()).unwrap().map(|m| m.content == text).unwrap_or(false)
        };
        assert!(count(&store, "one"));
        // A snapshot st calls stale (another host's): a fresh one is read and the message goes once.
        let other = "snapshot/another-host/1/0000000000000000".to_owned();
        let second = send_message(&client, "agent/example/worker", "two".into(), None, None, None, vec![], vec![], None, Some(other))
            .await
            .unwrap()
            .expect("the message went after a fresh snapshot");
        assert_ne!(Some(second.as_str()), first.as_deref(), "a new message, once");
        assert_eq!(store.message(&second).unwrap().unwrap().content, "two");
        server.abort();
    }

    #[tokio::test]
    async fn sidebar_register_edits_merge_and_survive_daemon_restart() {
        use std::sync::Arc;
        use st3_client::ArrangementOperation as Op;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("sidebar.sock");
        async fn serve(root: &std::path::Path, socket: &std::path::Path) -> (Arc<st3::store::Store>, tokio::task::JoinHandle<()>) {
            let store = Arc::new(st3::store::Store::open(&root.join("graph.db"), "sidebar-edit-test").unwrap());
            let state = st3::api::AppState {
                owning_person: None,
                store: store.clone(), notify: Arc::new(tokio::sync::Notify::new()), event_notify: tokio::sync::watch::channel(0_u64).0,
                node: "sidebar-edit-test".into(), state_dir: root.into(), pty_root: root.join("pty"), pty_binary: "pty".into(),
                fleet_id: None, configured_peers: vec![], client_relay: None, native_session_home: None, planner_default: Default::default(),
            };
            let path = socket.to_owned();
            let server = tokio::spawn(async move { st3::api::serve_unix(&path, st3::api::router(state)).await.unwrap(); });
            for _ in 0..200 { if socket.exists() { break; } tokio::time::sleep(Duration::from_millis(5)).await; }
            (store, server)
        }
        let (store, server) = serve(root.path(), &socket).await;
        let client = Client::unix_as(&socket, "person/avery");
        let other_device = Client::unix_as(&socket, "person/avery");
        let subject = format!("arrangement/person/avery/{}", uuid::Uuid::now_v7());
        let first = uuid::Uuid::now_v7().to_string(); let second = uuid::Uuid::now_v7().to_string();
        let write = |operations| super::super::resource_sidebar::Write { subject: subject.clone(), owner: "person/avery".into(), operations, key: uuid::Uuid::now_v7().to_string(), creating: false };
        let creation = write(vec![Op::Create { name: "Sidebar".into() }, Op::FolderCreate { id: first.clone(), name: "First".into(), parent: None, key: "a0".into() }, Op::FolderCreate { id: second.clone(), name: "Second".into(), parent: None, key: "a1".into() }, Op::SubjectPlace { subject: "agent/example/atlas".into(), folder: Some(first.clone()), key: "a0".into() }]);
        save_sidebar(&client, &creation).await.unwrap();
        // Distinct registers edited by two devices combine instead of replacing a layout.
        save_sidebar(&other_device, &write(vec![Op::FolderRename { id: first.clone(), name: "Renamed".into() }])).await.unwrap();
        save_sidebar(&client, &write(vec![Op::FolderMove { id: second.clone(), parent: None, key: "Zz".into() }, Op::SubjectPlace { subject: "agent/example/atlas".into(), folder: Some(second.clone()), key: "a1".into() }])).await.unwrap();
        // Exact retry of the accepted create does not replace later register winners.
        save_sidebar(&client, &creation).await.unwrap();
        let found = client.arrangements_get("avery", subject.rsplit('/').next().unwrap()).await.unwrap().value;
        assert_eq!(found.body.folders[&first].name.value, "Renamed");
        assert!(found.body.folders[&second].position.value.key < found.body.folders[&first].position.value.key);
        assert_eq!(found.resolved.as_ref().unwrap().folders["agent/example/atlas"], Some(second.clone()));
        save_sidebar(&client, &write(vec![Op::FolderDelete { id: second.clone() }])).await.unwrap();
        server.abort(); let _ = server.await; drop(store);
        let _ = std::fs::remove_file(&socket);
        let (_store, server) = serve(root.path(), &socket).await;
        let found = client.arrangements_get("avery", subject.rsplit('/').next().unwrap()).await.unwrap().value;
        assert_eq!(found.body.folders[&first].name.value, "Renamed");
        assert!(found.body.folders[&second].tombstone.is_some());
        assert_eq!(found.resolved.as_ref().unwrap().folders["agent/example/atlas"], None);
        // The unavailable subject remains placed in the registers; deleting a folder never deletes a resource.
        assert!(found.body.placements.contains_key("agent/example/atlas"));
        server.abort();
    }

    #[tokio::test]
    async fn live_sends_retries_discussions_and_creation_survive_daemon_restarts() {
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("daemon.sock");
        async fn serve(
            root: &std::path::Path,
            socket: &std::path::Path,
        ) -> (Arc<st3::store::Store>, tokio::task::JoinHandle<()>) {
            let store =
                Arc::new(st3::store::Store::open(&root.join("graph.db"), "ui-actions").unwrap());
            let state = st3::api::AppState {
                owning_person: None,
                store: store.clone(),
                notify: Arc::new(tokio::sync::Notify::new()),
                event_notify: tokio::sync::watch::channel(0_u64).0,
                node: "ui-actions".into(),
                state_dir: root.into(),
                pty_root: root.join("pty"),
                pty_binary: "pty".into(),
                fleet_id: None,
                configured_peers: vec![],
                client_relay: None,
                native_session_home: None,
                planner_default: Default::default(),
            };
            let path = socket.to_owned();
            let server = tokio::spawn(async move {
                st3::api::serve_unix(&path, st3::api::router(state))
                    .await
                    .unwrap();
            });
            for _ in 0..100 {
                if socket.exists() {
                    Client::unix_as(socket, "person/avery")
                        .capabilities()
                        .await
                        .unwrap();
                    return (store, server);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("private UI daemon did not listen");
        }
        let (mut store, mut server) = serve(root.path(), &socket).await;
        let client = Client::unix_as(&socket, "person/avery");
        let model = Model::default();
        let image = root.path().join("copper.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nproof").unwrap();
        let sent = Mutex::new(None);
        let send = Effect::Send {
            in_reply_to: None,
            agent: "agent/example/worker".into(),
            text: "Copper proof".into(),
            tags: vec![],
            images: vec![image],
        };
        let (_, message) = perform(&client, "person/avery", &model, send.clone(), Some(&sent))
            .await
            .unwrap();
        let message = message.unwrap();
        assert_eq!(
            store.message(&message).unwrap().unwrap().attachments.len(),
            1
        );
        let index = store.index().unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        std::fs::remove_file(&socket).unwrap();
        (store, server) = serve(root.path(), &socket).await;
        // This is the live loop's resend path: replay the preserved request first.
        let replay = send_message(
            &client,
            "agent/example/worker",
            "Copper proof".into(),
            None,
            None,
            None,
            vec![],
            store
                .message(&message)
                .unwrap()
                .unwrap()
                .attachments
                .iter()
                .map(|a| st3_client::AttachmentInput {
                    blob: format!("blob/{}", a.sha256),
                    media_type: a.media_type.clone(),
                    name: a.name.clone(),
                })
                .collect(),
            Some(&sent),
            None,
        )
        .await
        .unwrap();
        assert_eq!(replay.as_deref(), Some(message.as_str()));
        assert_eq!(store.index().unwrap(), index);
        let reply_sent = Mutex::new(None);
        let reply = Effect::Send {
            agent: "agent/example/worker".into(),
            text: "Thanks for the copper proof.".into(),
            in_reply_to: Some(message.clone()),
            tags: vec![],
            images: vec![],
        };
        let (_, replied) = perform(
            &client,
            "person/avery",
            &model,
            reply.clone(),
            Some(&reply_sent),
        )
        .await
        .unwrap();
        let replied = replied.unwrap();
        assert_eq!(
            store
                .message(&replied)
                .unwrap()
                .unwrap()
                .in_reply_to
                .as_deref(),
            Some(message.as_str())
        );
        let reply_index = store.index().unwrap();
        let (_, repeated) = perform(&client, "person/avery", &model, reply, Some(&reply_sent))
            .await
            .unwrap();
        assert_eq!(repeated.as_deref(), Some(replied.as_str()));
        assert_eq!(
            store.index().unwrap(),
            reply_index,
            "threaded replies keep the same retry receipt"
        );
        for effect in [
            Effect::Discuss {
                to: "agent/example/worker".into(),
                title: "Copper discussion".into(),
                text: "Record the evidence".into(),
            },
            Effect::CreateTerminal {
                name: "Copper shell".into(),
            },
            Effect::CreateLaunch {
                title: "Copper launch".into(),
                request: "Prepare the copper proof".into(),
                mission: "mission/example/copper".into(),
                workspace: root.path().display().to_string(),
            },
        ] {
            server.abort();
            assert!(server.await.unwrap_err().is_cancelled());
            std::fs::remove_file(&socket).unwrap();
            (store, server) = serve(root.path(), &socket).await;
            perform(&client, "person/avery", &model, effect, None)
                .await
                .unwrap();
        }
        assert_eq!(store.planning_sessions(true).unwrap().len(), 1);
        let launch = store.planning_sessions(true).unwrap().pop().unwrap();
        let transport = st3::client::Client::unix_as(&socket, "person/avery").unwrap();
        let _: serde_json::Value = transport.post(&format!("/v1/launches/{}/variants/default/submit", launch.id), &st3::model::PlanningCandidateSubmitRequest {
            actor: launch.planner.clone(), markdown: b"Copper proof".to_vec(),
            kdl: b"version 2\nmission \"example/copper\" state=\"ready\" { goal \"Record the copper proof.\"; step \"proof\" { agentless } }\n".to_vec(),
            idempotency_key: "ui-prepare-candidate".into(),
        }).await.unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        std::fs::remove_file(&socket).unwrap();
        (store, server) = serve(root.path(), &socket).await;
        let attention: serde_json::Value = transport.get("/v1/client/attention").await.unwrap();
        let review = attention["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["source_id"] == launch.subject)
            .unwrap();
        perform(
            &client,
            "person/avery",
            &model,
            Effect::LaunchRevise {
                id: review["id"].as_str().unwrap().into(),
                feedback: "Name the copper evidence".into(),
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            store.planning_session(&launch.id).unwrap().unwrap().status,
            "revision-requested"
        );
        let challenge: serde_json::Value = transport.post("/v1/client/pairings", &serde_json::json!({"api_version": "st3.client.v0", "device_name": "Copper phone", "person_id": "person/avery", "full_control": true})).await.unwrap();
        let paired: serde_json::Value = transport.post(&format!("/v1/client/pairings/{}/complete", challenge["pairing_id"].as_str().unwrap().trim_start_matches("pairing/")), &serde_json::json!({"api_version": "st3.client.v0", "code": challenge["code"], "device_public_key": "copper-phone-key-000000000000000000000000"})).await.unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        std::fs::remove_file(&socket).unwrap();
        (store, server) = serve(root.path(), &socket).await;
        perform(
            &client,
            "person/avery",
            &model,
            Effect::RevokeDevice {
                id: paired["device_id"].as_str().unwrap().into(),
            },
            None,
        )
        .await
        .unwrap();
        assert!(
            Client::unix_gateway(&socket, paired["credential"].as_str().unwrap())
                .capabilities()
                .await
                .is_err()
        );
        // A retired/changed Home message card cannot accidentally send a reply.
        let index = store.index().unwrap();
        assert!(
            perform(
                &client,
                "person/avery",
                &model,
                Effect::Reply {
                    id: "attention/absent-card".into(),
                    to: "agent/example/worker".into(),
                    text: "Copper reply".into()
                },
                None
            )
            .await
            .is_err()
        );
        assert_eq!(store.index().unwrap(), index);
        let source = "version 2\nmission \"example/cancel\" state=\"ready\" { goal \"Record the proof.\"; step \"proof\" { agentless } }\n";
        let intent = st3::parse_intent(source, "ui-actions").unwrap();
        let preview = store
            .mission(
                &intent,
                st3::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        store
            .apply_as(
                &intent,
                &preview.subject_tokens,
                "ui-cancel-definition",
                Some("person/avery"),
            )
            .unwrap();
        let run = store
            .create_mission_run(&st3::model::MissionRunRequest {
                mission: "example/cancel".into(),
                revision: None,
                workspace: root.path().display().to_string(),
                requester: Some("person/avery".into()),
                mode: Some("run".into()),
                inputs: BTreeMap::new(),
                idempotency_key: "ui-cancel-run".into(),
            })
            .unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        std::fs::remove_file(&socket).unwrap();
        (store, server) = serve(root.path(), &socket).await;
        // The missions as st lists them, as the feed would bring them.
        let mut current = Model::default();
        current.missions.items = client
            .missions_list(None, Some(100), false)
            .await
            .unwrap()
            .value
            .items;
        perform(
            &client,
            "person/avery",
            &current,
            Effect::CancelRun {
                mission: "mission/example/cancel".into(),
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            store
                .step_run(&run.steps[0].subject)
                .unwrap()
                .unwrap()
                .status,
            "cancelled"
        );
        server.abort();
    }

    /// A stale fence is retried for an action that reads a fresh one each try, but not for an
    /// attention action: it already moved once to its source's current card. A busy or absent
    /// st is retried for both.
    #[test]
    fn an_attention_action_is_not_retried_on_a_stale_fence() {
        let refused = |code: st3_client::ErrorCode| {
            anyhow::Error::new(ClientError::Api(
                code.clone(),
                "refused".into(),
                Box::new(st3_client::ErrorEnvelope {
                    api_version: "st3.client.v0".into(),
                    error_version: "st3.client.error.v0".into(),
                    request_id: "request/1".into(),
                    code,
                    message: "refused".into(),
                    retryable: false,
                    retry_after_ms: None,
                    details: BTreeMap::new(),
                }),
            ))
        };
        let attention = Effect::Attention {
            id: "attention/one".into(),
            action: "review.approve".into(),
            reason: None,
            answer: None,
        };
        let terminal = Effect::CreateTerminal {
            name: "shell".into(),
        };
        let stale = refused(st3_client::ErrorCode::StaleFence);
        assert!(!not_applied(&stale, &attention));
        assert!(not_applied(&stale, &terminal));
        let busy = refused(st3_client::ErrorCode::RateLimited);
        assert!(not_applied(&busy, &attention));
        assert!(not_applied(&busy, &terminal));
        assert!(!not_applied(
            &refused(st3_client::ErrorCode::ValidationFailed),
            &attention
        ));
    }

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
        receipts.completed("message/one".into(), ReceiptOutcome::Retry, now);
        assert_eq!(receipts.next(now).as_deref(), Some("message/two"));
        receipts.completed("message/two".into(), ReceiptOutcome::Done, now);
        assert!(receipts.next(now).is_none(), "retry backs off");
        assert_eq!(
            receipts.next(now + Duration::from_secs(2)).as_deref(),
            Some("message/one"),
            "retry without another displayed frame"
        );
        receipts.completed("message/one".into(), ReceiptOutcome::Done, now);
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
                signed: None,
                images: Vec::new(),
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
            owning_person: None,
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
                signed: None,
                images: Vec::new(),
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

    #[test]
    fn failed_conversation_hint_and_key_resubscribe_without_resending_messages() {
        use super::super::{demo, glass, Hit};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for (spaces, child) in [(false, false), (true, false), (true, true)] {
            let mut ui = Ui::new(demo::world());
            ui.live = true;
            ui.tab = 1;
            let selected = ui.selected_id().unwrap();
            if spaces {
                ui.glasses = Some(glass::Glasses::open(None, None));
            }
            let target = if child {
                ui.click(Hit::Pane(st3_conversation_ui::PaneIntent::Open(
                    "session/tester-child".into(),
                )));
                "session/tester-child".to_owned()
            } else {
                let target = selected;
                if spaces {
                    ui.click(Hit::Pane(st3_conversation_ui::PaneIntent::Open(target.clone())));
                }
                target
            };
            let mut failed = BTreeMap::from([(target.clone(), "access refused".into())]);
            let mut requested = HashSet::from([target.clone()]);
            ui.world.conversations = conversations(
                &Model::default(),
                "person/ada",
                &BTreeMap::new(),
                &failed,
                std::slice::from_ref(&target),
            );
            ui.effects.clear();
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
            terminal.draw(|frame| ui.render(frame)).unwrap();
            let shown: String = terminal.backend().buffer().content.iter()
                .map(ratatui::buffer::Cell::symbol).collect();
            assert!(shown.contains("access refused"), "{shown}");
            assert!(shown.contains("Ctrl+R · retry opening this conversation"), "{shown}");
            assert!(!shown.contains("retrying") && !shown.contains("Trying again"), "{shown}");
            assert!(ui.effects.is_empty(), "rendering a refusal never retries it");

            ui.conversation_state.drafts.insert(target.clone(), "saved draft".into());
            let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
            ui.editing = true;
            ui.key(key);
            assert!(ui.effects.is_empty(), "a draft owns Ctrl+R while the box is open");
            assert!(matches!(ui.world.conversations[&target], Load::Failed(_)));
            ui.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            ui.key(key);
            let effects = std::mem::take(&mut ui.effects);
            let [Effect::RetryConversation { target: reopened }] = effects.as_slice() else {
                panic!("opening is distinct from message resend: {effects:?}");
            };
            assert_eq!(reopened, &target);
            assert!(matches!(ui.world.conversations[&target], Load::Loading));
            assert!(!ui.stalled.contains_key(&target));
            assert_eq!(ui.conversation_state.drafts[&target], "saved draft");
            let (commands, mut incoming) = tokio::sync::mpsc::unbounded_channel();
            retry_conversation_open(reopened.clone(), &mut failed, &mut requested, &commands);
            assert!(matches!(incoming.try_recv().unwrap(), Command::Resubscribe { target: sent } if sent == target));
            assert!(!failed.contains_key(&target));
            assert!(!requested.contains(&target));
            ui.key(key);
            assert!(ui.effects.is_empty(), "loading does not queue another explicit retry");
            assert!(incoming.try_recv().is_err(), "no automatic retry is sent");
        }
    }

    #[test]
    fn a_session_that_failed_to_open_says_why_and_is_asked_again_when_reopened() {
        let session = "session/example-child".to_owned();
        let failed = BTreeMap::from([(
            session.clone(),
            "not found · open it again to retry".to_owned(),
        )]);
        let shown = conversations(
            &Model::default(),
            "person/avery",
            &BTreeMap::new(),
            &failed,
            &[session.clone()],
        );
        match &shown[&session] {
            Load::Failed(reason) => assert!(
                reason.contains("not found") && reason.contains("retry"),
                "{reason}"
            ),
            other => panic!("a failed open is an error, not loading: {other:?}"),
        }
        let mut requested = HashSet::from([session.clone(), "session/example-open".to_owned()]);
        // On screen, the failure stays and is not asked again in a loop.
        retry_failed_opens(&mut requested, &failed, std::slice::from_ref(&session));
        assert!(requested.contains(&session));
        // Left, it is forgotten, so opening it again asks st once more; a good open is kept.
        retry_failed_opens(&mut requested, &failed, &[]);
        assert!(!requested.contains(&session));
        assert!(requested.contains("session/example-open"));
    }

    #[test]
    fn a_conversation_says_above_its_oldest_entry_how_far_back_it_goes() {
        let items: Vec<st3_client::TimelineEntry> = serde_json::from_value(serde_json::json!([
            {"id":"c","sequence":2,"revision":1,"timestamp":"2026-10-01T10:00:00Z","role":"assistant","type":"content","final":true,
             "body":{"media_type":"text/plain","text":"The audit is done."}},
        ]))
        .unwrap();
        let target = "agent/example/harbor/keeper";
        let note = |timeline: st3_conversation_ui::Timeline| {
            let shown = conversations(
                &Model::default(),
                "person/avery",
                &BTreeMap::from([(target.to_owned(), timeline)]),
                &BTreeMap::new(),
                &[target.to_owned()],
            );
            let Load::Ready(entries) = &shown[target] else {
                panic!("expected entries");
            };
            match &entries[0].body {
                st3_conversation_ui::Body::Event(text) => text.clone(),
                _ => String::new(),
            }
        };
        let mut timeline = st3_conversation_ui::Timeline {
            items: items.clone(),
            has_more: true,
            ..Default::default()
        };
        assert_eq!(note(timeline), "Scroll up for earlier entries");
        timeline = st3_conversation_ui::Timeline {
            items: items.clone(),
            has_more: true,
            ..Default::default()
        };
        timeline.older.loading = true;
        assert_eq!(note(timeline), "Loading earlier entries…");
        timeline = st3_conversation_ui::Timeline {
            items: items.clone(),
            has_more: true,
            ..Default::default()
        };
        timeline.older_failed("st did not answer".into());
        assert!(note(timeline).starts_with("Could not load earlier entries: st did not answer"));
        timeline = st3_conversation_ui::Timeline {
            items,
            ..Default::default()
        };
        assert!(note(timeline).starts_with("Start of this session"));
    }

    #[test]
    fn a_restarted_agents_conversation_follows_its_new_session_once() {
        let target = "agent/example/harbor/keeper".to_owned();
        let mut model = Model::default();
        model.agents.items.push(
            serde_json::from_str(r#"{"kind":"agent","id":"agent/example/harbor/keeper","revision":"a","updated_at":"2026-10-03T08:28:00Z","name":"Keeper","state":"running","reachability":"reachable","current_session_id":"session/new"}"#)
                .unwrap(),
        );
        let mut timelines = BTreeMap::new();
        let mut timeline = st3_conversation_ui::Timeline::default();
        timeline.apply(st3_conversation_ui::Frame {
            replace: true,
            session_id: Some("session/old".into()),
            ..Default::default()
        });
        timelines.insert(target.clone(), timeline);
        let mut asked = BTreeMap::new();
        let conversing = [target.clone()];
        assert_eq!(
            moved_sessions(&conversing, &model, &timelines, &mut asked),
            std::slice::from_ref(&target)
        );
        // Asked once for that session, not on every pass.
        assert!(moved_sessions(&conversing, &model, &timelines, &mut asked).is_empty());
        // Once the conversation shows the new session, nothing more is asked.
        timelines.get_mut(&target).unwrap().apply(st3_conversation_ui::Frame {
            replace: true,
            session_id: Some("session/new".into()),
            ..Default::default()
        });
        asked.clear();
        assert!(moved_sessions(&conversing, &model, &timelines, &mut asked).is_empty());
    }

    #[test]
    fn a_message_st_has_not_answered_becomes_unconfirmed_so_it_can_go_again() {
        let pending = |since: Instant| Pending {
            token: "t".into(),
            agent: "agent/example/cos".into(),
            text: "hello".into(),
            at: "10:29".into(),
            message_id: None,
            failed: None,
            unconfirmed: false,
            effect: Effect::Send {
                in_reply_to: None,
                agent: "agent/example/cos".into(),
                text: "hello".into(),
                tags: Vec::new(),
                images: Vec::new(),
            },
            sent: Arc::new(Mutex::new(None)),
            echoed: false,
            since,
        };
        let mut fresh = [pending(Instant::now())];
        assert!(!mark_unanswered(&mut fresh));
        assert!(fresh[0].failed.is_none());
        let mut waiting = [pending(Instant::now() - UNANSWERED_AFTER)];
        assert!(mark_unanswered(&mut waiting));
        assert!(waiting[0].unconfirmed && waiting[0].failed.is_some());
        // Said once.
        assert!(!mark_unanswered(&mut waiting));
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
    /// How it was reached, for the person: `local PTY`, `Fabric to HOST`, or `via the daemon (…)`.
    route: String,
}

/// A direct stream to `subject`'s PTY session: st's raw terminal stream, fenced to the
/// terminal's incarnation and routed to the host that owns it. `subject` is an agent (its
/// terminal is the first of `runtime_ids` that has one) or a shell's terminal. With `expected`,
/// only that incarnation: a terminal that restarted since is not quietly swapped in.
/// Attach straight to the terminal st already named, the quick way (each runtime read is a full
/// round trip to the daemon, a second or more on a busy one: Nathan, 2026-10-04, "attaching
/// … feels kinda slow"); when that terminal is not there, find it through the agent's runtimes.
/// What a terminal that is still connecting says: nothing special at first, then how long it has
/// waited, then that st is the slow part.
fn connecting_text(waited: Duration) -> String {
    match waited.as_secs() {
        0..2 => "connecting".into(),
        2..8 => format!("connecting {}s", waited.as_secs()),
        seconds => format!("connecting {seconds}s, st is slow to answer"),
    }
}

/// How long a shell's attach keeps trying after st refuses it for a reason that passes: about a
/// minute and a half in all.
const ATTACH_RETRY_DELAYS: [Duration; 20] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(3),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
];

/// Whether an attach refused with `reason` may work a moment later: st cannot tell yet which host
/// runs the terminal, that host is briefly out of reach, or st asked for a pause. A terminal that
/// is gone, restarted or not allowed is not retried.
fn worth_retrying(reason: &str) -> bool {
    [
        "cannot tell yet which host",
        "cannot be reached right now",
        "asked to slow down",
        "changed while this was on its way",
    ]
    .iter()
    .any(|passing| reason.contains(passing))
}

/// Try `attempt` again after each delay while it fails for a reason that passes, telling `waiting`
/// the reason before each wait; the last refusal comes back once the delays are used up.
async fn retry_attach<T, Fut>(
    delays: &[Duration],
    mut waiting: impl FnMut(&str),
    mut attempt: impl FnMut() -> Fut,
) -> Result<T, String>
where
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut delays = delays.iter();
    loop {
        match attempt().await {
            Err(reason) if worth_retrying(&reason) => match delays.next() {
                Some(delay) => {
                    waiting(&reason);
                    tokio::time::sleep(*delay).await;
                }
                None => return Err(reason),
            },
            done => return done,
        }
    }
}

async fn attach_known_then_direct(
    client: &Client,
    agent: &str,
    known: Option<(String, String, String)>,
    runtime_ids: &[String],
    owner: Option<&str>,
    fallback: bool,
) -> Result<Direct, String> {
    if let Some((name, terminal, incarnation)) = known
        && !agent.starts_with("terminal/")
    {
        // Once selected, keep this incarnation through every route failure. Rediscovery after
        // an error could silently attach a process that restarted in the meantime.
        return attach_terminal(
            client, agent, (name, incarnation), &terminal, owner, None, fallback,
        ).await;
    }
    attach_direct(client, agent, runtime_ids, None, owner, fallback).await
}

async fn attach_direct(
    client: &Client,
    subject: &str,
    runtime_ids: &[String],
    expected: Option<&str>,
    owner: Option<&str>,
    fallback: bool,
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
            owner.map(str::to_owned),
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
                found.push((
                    runtime.runtime_id,
                    terminal,
                    incarnation,
                    Some(runtime.owner_host_id).filter(|host| !host.is_empty()),
                ));
            }
        }
    }
    let mut reason = "the agent has no terminal right now".to_owned();
    for (name, terminal, incarnation, runtime_owner) in found {
        if expected.is_some_and(|expected| expected != incarnation) {
            reason = direct::RESTARTED.into();
            continue;
        }
        let owner = owner.map(str::to_owned).or(runtime_owner);
        match attach_terminal(
            client,
            subject,
            (name, incarnation),
            &terminal,
            owner.as_deref(),
            expected,
            fallback,
        )
        .await
        {
            Ok(direct) => return Ok(direct),
            Err(error) => reason = error,
        }
    }
    Err(reason)
}

/// One terminal's PTY session at one incarnation, and its stream: the local socket for a terminal
/// on this host, Fabric for one another fleet host owns. The paired-device gateway carries the
/// bytes only for a device with no daemon of its own.
async fn attach_terminal(
    client: &Client,
    subject: &str,
    identity: (String, String),
    terminal: &str,
    owner: Option<&str>,
    expected: Option<&str>,
    fallback: bool,
) -> Result<Direct, String> {
    let (name, incarnation) = identity;
    let opened = direct::open(
        client,
        &direct::Terminal {
            subject,
            name: &name,
            incarnation: &incarnation,
            owner,
            expected,
            fallback,
        },
    )
    .await?;
    let route = match opened {
        direct::Opened::Stream {
            stream,
            name,
            incarnation,
            route,
        } => {
            return Ok(Direct {
                name,
                incarnation,
                stream,
                route,
            });
        }
        direct::Opened::UseGateway { route } => route,
    };
    let attachment = client
        .raw_terminal_attachment(terminal, &incarnation, st3_client::RawTerminalMode::Attach)
        .await
        .map_err(|error| format!("{route}: {}", error.plain()))?;
    let stream = client
        .raw_terminal_stream(&attachment)
        .await
        .map_err(|error| format!("{route}: {}", error.plain()))?;
    stream
        .into_std()
        .map(|stream| Direct {
            name,
            incarnation,
            stream,
            route,
        })
        .map_err(|error| error.to_string())
}

/// The fleet host that owns `agent`'s terminal, as st already says it: the agent's host, or else
/// the host of the runtime that holds the terminal.
fn terminal_owner(
    model: &Model,
    agent: &str,
    found: Option<&st3_client::Agent>,
    runtime_ids: &[String],
) -> Option<String> {
    found
        .and_then(|agent| agent.host_id.clone())
        .filter(|host| !host.is_empty())
        .or_else(|| {
            model.runtimes.items.iter().find_map(|item| match item {
                Resource::Runtime(runtime)
                    if runtime.terminal_id.as_deref() == Some(agent)
                        || runtime_ids.contains(&runtime.header.id) =>
                {
                    Some(runtime.owner_host_id.clone()).filter(|host| !host.is_empty())
                }
                _ => None,
            })
        })
}
