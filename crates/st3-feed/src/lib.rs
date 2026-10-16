//! One socket to st for live collection windows, conversations and a terminal.
//!
//! The feed holds one collection socket with the attention, missions and agents windows, the
//! open conversations, and, while a terminal is open, that terminal. st joins each row and each
//! conversation, so callers never join lists themselves or read item by item. When the socket drops,
//! the feed opens a new one, subscribes again, and attaches the open terminal again. Nothing
//! here polls projections while connected: frames arrive only when something changed. Remote
//! devices use the protocol heartbeat so an idle network blackhole becomes offline.

#![doc = include_str!("../README.md")]

pub mod cache;
pub mod model;
mod inventory;
mod terminal;
pub mod tree;

pub use terminal::{action_pair, terminal_fence, terminal_screen_fence};
pub use inventory::{Inventories, Inventory};

use st3_client::{
    CapabilityState, Client, ClientError, CollectionEvent, CollectionStream, ErrorCode, Fence,
    Resource, Snapshot, TargetParameters, TerminalScreen, TimelineEntry,
};
use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc as channel;
use tokio::time::Instant;

/// How many current items each window holds. st sends at most 200.
pub const WINDOW: usize = 200;

/// How often the stream's own quiet time is looked at; nothing is sent to look.
const QUIET_CHECK: Duration = Duration::from_secs(2);
/// How long a stream may say nothing before it is pinged.
const QUIET_BEFORE_PING: Duration = Duration::from_secs(10);
/// How long after a ping its pong, or anything else, may take before the socket is dropped.
const PONG_WAIT: Duration = Duration::from_secs(8);

/// The waits between attempts to reach st again, reset once st answers.
/// How long a window must stay unserved before st's reason for it is reported. A resync or a
/// slow answer from a daemon that is catching up clears itself within this; saying it sooner puts
/// an error in front of a person for something that mends before they could read it.
pub const REPORT_AFTER: Duration = Duration::from_secs(20);

const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// How long a fresh request may take to prove st is answering after its stream dropped.
const PROBE_WAIT: Duration = Duration::from_secs(3);

/// The subscription ID of the open terminal.
const TERMINAL: &str = "terminal";
/// The subscription ID of the open conversation.
const CONVERSATION: &str = "conversation";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Window {
    Attention,
    Missions,
    Agents,
    /// The person's glasses, followed when requested and granted by st.
    Glasses,
    /// A handful of counts (needs you, working agents, active missions, machines), followed when
    /// st grants it: what the top bar says without following the whole missions window.
    Summary,
    /// The selected person’s sidebar organization, when supported.
    Arrangements,
    Inventory(Inventory),
}

impl Window {
    pub const ALL: [Self; 3] = [Self::Attention, Self::Missions, Self::Agents];

    fn id(self) -> &'static str {
        match self {
            Self::Attention => "attention",
            Self::Missions => "missions",
            Self::Agents => "agents",
            Self::Glasses => "glasses",
            Self::Summary => "summary",
            Self::Arrangements => "arrangements",
            Self::Inventory(inventory) => inventory.collection(),
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .chain([Self::Glasses, Self::Summary, Self::Arrangements])
            .find(|window| window.id() == id)
            .or_else(|| Inventory::from_id(id).map(Self::Inventory))
    }

    /// How many items to follow: every glass a person may keep, a page of anything else.
    fn limit(self) -> usize {
        match self {
            Self::Glasses | Self::Arrangements => GLASSES,
            _ => WINDOW,
        }
    }
}

/// The glasses capability version whose glasses are splits of tab groups.
const GLASSES_VERSION: u32 = 1;

/// The most glasses st keeps live for one person.
const GLASSES: usize = 100;

#[derive(Debug)]
pub enum Update {
    /// Use this member for actions too. A live window must arrive before actions are enabled.
    Connected(Client),
    /// st cannot be reached; the feed keeps trying and every window keeps its last items.
    Offline(String),
    /// st answers a fresh request, but the live stream to it dropped or would not open, with this
    /// reason. The feed opens another after its usual short wait and every window keeps its last
    /// items: the link is not offline, and the reason is shown rather than a red word that is not
    /// true. Only a daemon on this machine is probed.
    Degraded(String),
    /// The granted glasses shape, delivered before its window. The embedding UI chooses how
    /// to store glasses for this version; the feed owns no application state.
    GlassesVersion(u32),
    /// Optional inventory admission and the member build, discovered once per connection.
    InventorySupport { granted: bool, member_build: Option<String> },
    /// A window's current items, in st's display order.
    Window {
        /// Client monotonic receipt time, before incremental view maintenance.
        received: std::time::Instant,
        window: Window,
        snapshot: Snapshot,
        items: Vec<Resource>,
        has_more: bool,
    },
    /// st refused a window; its last items stay.
    WindowFailed(Window, String),
    /// st's window is no longer followed (a Missions list left the screen): the rows held for it
    /// are out of date and must not be shown as current.
    WindowStopped(Window),
    Terminal(TerminalUpdate),
    /// The open conversation's entries. `replace` means these are its newest page and every
    /// earlier entry is gone; otherwise they are new or revised entries, matched by ID.
    Conversation {
        target: String,
        /// The session the entries belong to: earlier pages are read from its timeline.
        session_id: String,
        replace: bool,
        /// Availability before the live window; absent on deltas means unchanged.
        has_more: Option<bool>,
        items: Vec<TimelineEntry>,
        /// The conversation header (contract §3) when this frame carries one.
        header: Option<serde_json::Value>,
    },
    /// st could not show the open conversation; the feed asks again after a backoff, unless
    /// st said it never can (`permanent`).
    ConversationFailed {
        target: String,
        /// Typed refusal for content-free client error accounting.
        code: Option<ErrorCode>,
        message: String,
        permanent: bool,
    },
}

#[derive(Debug)]
pub enum TerminalUpdate {
    /// The viewer record input and detach are fenced to.
    Attached {
        terminal_id: String,
        attachment_id: String,
        incarnation: String,
    },
    Screen(Box<TerminalScreen>),
    /// The stream dropped. The last screen stays, marked stale, while the feed attaches again.
    Reconnecting(String),
    /// Following stopped. `restarted` means the terminal restarted or its process exited, which
    /// only reopening it resolves.
    Ended {
        restarted: bool,
        reason: String,
    },
}

#[derive(Debug)]
pub enum Command {
    /// Retry a connection now; never a queued mutation.
    Reconnect,
    /// Follow this agent's terminal, found among its runtimes, in place of any other.
    Follow {
        runtime_ids: Vec<String>,
    },
    Unfollow,
    /// Follow the missions window or leave it: the whole window is large, so it is followed only while
    /// a Missions list is on screen. A feed started with `run_members` follows it from the start.
    Missions {
        follow: bool,
    },
    /// Hold exactly these optional screen lists; the UI owns its leave grace.
    Inventories(Inventories),
    /// Keep exactly these agents' or sessions' conversations live, at most
    /// `MAX_CONVERSATIONS`: the first is the focused one. An empty list follows none.
    Converse {
        targets: Vec<String>,
    },
    /// Subscribe to a followed agent's conversation again: st resolves an agent to its session
    /// when the subscription starts, so after the agent restarts into a new session the old
    /// subscription would keep showing the old one.
    Resubscribe {
        target: String,
    },
}

/// Three conversations remain live alongside collection windows, glasses and a terminal.
/// Optional arrangements require the advertised sixteen-subscription socket capacity.
pub const MAX_CONVERSATIONS: usize = 3;

/// A conversation being shown, on its own subscription.
struct Conversing {
    target: String,
    /// A bounded correlation id, unique to this opening and reused for its retries.
    id: String,
    /// When to subscribe again after a failure; `None` while subscribed.
    retry_at: Option<Instant>,
    failures: usize,
}

/// The terminal being followed.
struct Following {
    runtime_id: String,
    terminal_id: String,
    /// The incarnation first attached. A different one means the terminal restarted.
    incarnation: Option<String>,
    attachment_id: Option<String>,
    /// When to attach again after a transient failure; `None` while subscribed.
    retry_at: Option<Instant>,
    failures: usize,
}

/// Follow one local member's core windows, conversations and terminal without glasses.
pub async fn run(
    client: Client,
    updates: mpsc::Sender<Update>,
    commands: channel::UnboundedReceiver<Command>,
) {
    run_members(vec![client], false, false, updates, commands).await;
}

/// Follow the core windows on one active member, trying each supplied client before backing off.
/// `glasses` requests the optional granted glasses window. `remote` identifies a device gateway
/// for connection diagnostics. Dropping the command sender stops the feed.
pub async fn run_members(
    clients: Vec<Client>,
    remote: bool,
    glasses: bool,
    updates: mpsc::Sender<Update>,
    commands: channel::UnboundedReceiver<Command>,
) {
    run_members_with(clients, remote, glasses, true, updates, commands).await;
}

/// `run_members`, choosing whether the missions window is followed from the start. A UI that shows
/// missions only sometimes starts without it and sends `Command::Missions`.
pub async fn run_members_with(
    clients: Vec<Client>,
    remote: bool,
    glasses: bool,
    missions: bool,
    updates: mpsc::Sender<Update>,
    commands: channel::UnboundedReceiver<Command>,
) {
    run_members_with_sidebar(clients, remote, glasses, missions, None, updates, commands).await;
}

/// Follow an optional explicit person's sidebar without reducing live conversation capacity.
pub async fn run_members_with_sidebar(
    clients: Vec<Client>,
    remote: bool,
    glasses: bool,
    missions: bool,
    person: Option<String>,
    updates: mpsc::Sender<Update>,
    mut commands: channel::UnboundedReceiver<Command>,
) {
    if clients.is_empty() {
        return;
    }
    let mut missions = missions;
    let mut inventories = Inventories::default();
    let mut failures = 0_usize;
    let mut member = 0;
    let mut following: Option<Following> = None;
    let mut conversing: Vec<Conversing> = Vec::new();
    loop {
        // Try every paired member before waiting. Never forward a grant to another origin.
        let mut selected = None;
        let mut reason = "No member is reachable".to_owned();
        for offset in 0..clients.len() {
            let index = (member + offset) % clients.len();
            match tokio::time::timeout(Duration::from_secs(5), clients[index].collection_stream())
                .await
            {
                Ok(Ok(stream)) => {
                    selected = Some((index, stream));
                    break;
                }
                Ok(Err(error)) => reason = error.to_string(),
                Err(_) => reason = "Member connection timed out".into(),
            }
        }
        if let Some((index, mut stream)) = selected {
            member = index;
            let client = &clients[index];
            if updates.send(Update::Connected(client.clone())).is_err() {
                return;
            }
            match connected(
                client,
                remote,
                glasses,
                person.as_deref(),
                &mut stream,
                &updates,
                &mut commands,
                &mut following,
                &mut conversing,
                &mut missions,
                &mut failures,
                &mut inventories,
            )
            .await
            {
                Ended::Closed => return,
                Ended::Dropped(reason) => {
                    if updates.send(link_lost(client, remote, &reason).await).is_err() {
                        return;
                    }
                    if let Some(current) = following.as_mut() {
                        current.attachment_id = None;
                        current.retry_at = None;
                        let _ =
                            updates.send(Update::Terminal(TerminalUpdate::Reconnecting(reason)));
                    }
                    for current in &mut conversing {
                        current.retry_at = None;
                    }
                    if clients.len() > 1 {
                        member = (member + 1) % clients.len();
                    }
                }
            }
        } else if updates
            .send(link_lost(&clients[member % clients.len()], remote, &reason).await)
            .is_err()
        {
            return;
        }
        // Wait before trying again, still honouring an unfollow meanwhile.
        let delay = RETRY_DELAYS[failures.min(RETRY_DELAYS.len() - 1)];
        // Device gateways have no peer-online channel. Keep their retry ceiling short,
        // with jitter so clients returning together do not all reconnect at once.
        let jitter = Duration::from_millis((uuid::Uuid::now_v7().as_u128() % 500) as u64);
        failures = failures.saturating_add(1);
        let wake = tokio::time::sleep(delay + jitter);
        tokio::pin!(wake);
        loop {
            tokio::select! {
                () = &mut wake => break,
                command = commands.recv() => match command {
                    None => return,
                    Some(Command::Reconnect) => { failures = 0; break; }
                    Some(Command::Unfollow) => following = None,
                    Some(Command::Missions { follow }) => missions = follow,
                    Some(Command::Inventories(wanted)) => inventories = wanted,
                    Some(Command::Converse { targets }) => {
                        conversing = targets.into_iter().take(MAX_CONVERSATIONS).map(Conversing::new).collect();
                    }
                    // Not connected: the next connection subscribes afresh anyway.
                    Some(Command::Resubscribe { .. }) => {}
                    Some(Command::Follow { runtime_ids }) => {
                        following = None;
                        match resolve(&clients[member], &runtime_ids).await {
                            Ok((runtime_id, terminal_id)) => following = Some(Following {
                                runtime_id, terminal_id, incarnation: None, attachment_id: None, retry_at: None, failures: 0,
                            }),
                            Err(reason) => {
                                let _ = updates.send(Update::Terminal(TerminalUpdate::Ended { restarted: false, reason }));
                            }
                        }
                    }
                },
            }
        }
    }
}

/// What a lost stream means. st is offline only when a fresh request to it also fails; when it
/// answers, only the stream is being replaced (a slow or failed read of another machine shows on
/// the view that waits for it, not as the whole link going down).
///
/// Only a daemon on this machine is probed: each request to its socket connects afresh. A paired
/// device's gateway is reached over HTTP connections that can be pooled and outlive the route
/// that carried them, so an answer there proves nothing about the stream; it stays offline.
pub async fn link_lost(client: &Client, remote: bool, reason: &str) -> Update {
    let answered = !remote
        && tokio::time::timeout(PROBE_WAIT, client.capabilities())
            .await
            .is_ok_and(|outcome| outcome.is_ok());
    // A paired device keeps no daemon or replica state of its own, and writes none for this.
    if !remote {
        connection_log(reason, answered);
    }
    if answered {
        Update::Degraded(reason.to_owned())
    } else {
        Update::Offline(reason.to_owned())
    }
}

/// One line per lost stream of a local daemon in `$XDG_STATE_HOME/st3/stui/connection.log`, so a
/// link that keeps dropping leaves its reasons behind. The file is cut to its last half when it grows past 256 KB.
fn connection_log(reason: &str, answered: bool) {
    use std::io::Write as _;
    // Tests lose streams on purpose and must not write to the person's state.
    if cfg!(test) {
        return;
    }
    let Some(base) = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state"))
        })
    else {
        return;
    };
    let directory = base.join("st3").join("stui");
    let path = directory.join("connection.log");
    let _ = std::fs::create_dir_all(&directory);
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 256 * 1024)
        && let Ok(bytes) = std::fs::read(&path)
    {
        let _ = std::fs::write(&path, &bytes[bytes.len() / 2..]);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(
            file,
            "{} {} {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            if answered {
                "stream-lost st-answers"
            } else {
                "offline"
            },
            reason.replace('\n', " ")
        );
    }
}

enum Ended {
    /// The caller is closing.
    Closed,
    /// The socket dropped; open another.
    Dropped(String),
}

/// Serve one open socket until it drops.
async fn connected(
    client: &Client,
    remote: bool,
    glasses: bool,
    person: Option<&str>,
    stream: &mut CollectionStream,
    updates: &mpsc::Sender<Update>,
    commands: &mut channel::UnboundedReceiver<Command>,
    following: &mut Option<Following>,
    conversing: &mut Vec<Conversing>,
    missions: &mut bool,
    failures: &mut usize,
    inventories: &mut Inventories,
) -> Ended {
    // A blackholed network, or a wedged daemon, can leave a socket open and silent while the
    // views say live. Nothing is asked of st while the stream speaks: any frame is proof of
    // life. Once it has been quiet for a while, a WebSocket ping (answered without running a
    // request) is sent; only when neither a frame nor its pong arrives is the socket dropped,
    // so one slow answer from a busy daemon never rereads every window.
    let heard = stream.heard();
    let mut pinged: Option<Instant> = None;
    let mut probe = tokio::time::interval(QUIET_CHECK);
    probe.tick().await;
    // Glasses are followed only where st grants them in the shape this stui reads (splits of
    // tab groups, version 1); elsewhere stui keeps them on the device. A member still on the
    // earlier shape would send glasses this stui cannot decode, and that drops the connection.
    let capabilities = match client.capabilities().await {
        Ok(capabilities) => capabilities,
        Err(error) => return Ended::Dropped(error.plain()),
    };
    let version = if glasses {
        capabilities
                .value
                .capabilities
                .iter()
                .find(|capability| {
                    capability.id == "glasses"
                        && capability.version >= GLASSES_VERSION
                        && capability.state == CapabilityState::Granted
                })
                .map(|capability| capability.version)
    } else {
        None
    };
    let supports = |id: &str| {
        capabilities.value.capabilities.iter().any(|capability| {
            capability.id == id
                && capability.version >= 1
                && capability.state == CapabilityState::Granted
        })
    };
    // The summary exists only where st grants it; an older member has no such collection.
    let summary = supports("summary");
    let inventory_granted = supports("inventory-collections") && supports("collections");
    if updates.send(Update::InventorySupport {
        granted: inventory_granted,
        member_build: capabilities.value.machine_version.clone(),
    }).is_err() { return Ended::Closed; }
    // Older sockets are already full with glasses, a terminal and three conversations.
    let sidebar_person = person.filter(|person| {
        person.starts_with("person/") && supports("arrangements") && supports("collections")
    });
    // From version 2 a glass's splits keep their sizes in st.
    if let Some(version) = version
        && updates.send(Update::GlassesVersion(version)).is_err()
    {
        return Ended::Closed;
    }
    let granted = version.is_some();
    for window in Window::ALL
        .into_iter()
        .filter(|window| *window != Window::Missions || *missions)
        .chain(granted.then_some(Window::Glasses))
        .chain(summary.then_some(Window::Summary))
    {
        if let Err(error) = stream
            .subscribe(window.id(), window.id(), window.limit(), None, None)
            .await
        {
            return Ended::Dropped(error.to_string());
        }
    }
    if let Some(person) = sidebar_person
        && let Err(error) = stream.subscribe_arrangements("arrangements", person, GLASSES, None).await {
        return Ended::Dropped(error.to_string());
    }
    if inventory_granted {
        for inventory in inventories.windows() {
            if let Err(error) = inventory.subscribe(stream).await { return Ended::Dropped(error.plain()); }
        }
    }
    for current in conversing.iter_mut() {
        if let Err(error) = converse(stream, current).await {
            return Ended::Dropped(error.to_string());
        }
    }
    let mut windows = BTreeMap::<Window, BTreeMap<String, Resource>>::new();
    // A window st stopped (its first read failed) is asked for again after a backoff, so a list
    // never stays stale under a live connection.
    let mut window_retries = WindowRetries::default();
    // Reasons st gave for a window it cannot serve yet, said once they have lasted a moment: a
    // write briefly revokes a source's readiness, and that blip is not worth a message.
    let mut pending_reports = PendingReports::default();
    if following.is_some() {
        match follow(client, stream, updates, following).await {
            Ok(()) => {}
            Err(error) => return Ended::Dropped(error.to_string()),
        }
    }
    loop {
        let retry_at = following.as_ref().and_then(|current| current.retry_at);
        let converse_at = conversing
            .iter()
            .filter_map(|current| current.retry_at)
            .min();
        let window_at = window_retries.next();
        let report_at = pending_reports.next();
        tokio::select! {
            _ = probe.tick() => {
                let quiet = heard.quiet_for();
                if quiet < QUIET_BEFORE_PING {
                    pinged = None;
                    *failures = 0;
                } else {
                    match pinged {
                        None => {
                            if let Err(error) = stream.ping().await {
                                return Ended::Dropped(error.plain());
                            }
                            pinged = Some(Instant::now());
                        }
                        Some(at) if at.elapsed() >= PONG_WAIT => {
                            return Ended::Dropped(if remote {
                                "the member stopped answering".into()
                            } else {
                                "st stopped answering".into()
                            });
                        }
                        Some(_) => {}
                    }
                }
            }
            event = stream.next_event() => {
                let event = match event {
                    Ok(Some(event)) => event,
                    Ok(None) => return Ended::Dropped("st closed the connection".into()),
                    Err(error) => return Ended::Dropped(error.to_string()),
                };
                let received = std::time::Instant::now();
                *failures = 0;
                match event {
                    CollectionEvent::Snapshot { id, snapshot, items, order, has_more } => {
                        let Some(window) = Window::from_id(&id) else { continue };
                        // A frame already on its way when the window was left: the socket was
                        // heard from, but nothing of the window comes back.
                        if left(window, *missions) || matches!(window, Window::Inventory(inventory) if !inventory_granted || !inventories.contains(inventory)) {
                            continue;
                        }
                        *failures = 0;
                        window_retries.loaded(window);
                        pending_reports.cancel(window);
                        let rows = windows.entry(window).or_default();
                        rows.clear();
                        rows.extend(items.into_iter().map(|item| (item.header().id.clone(), item)));
                        if !send_window(updates, received, window, snapshot, rows, &order, has_more) {
                            return Ended::Closed;
                        }
                    }
                    CollectionEvent::Changes { id, snapshot, upserts, removes, order, has_more } => {
                        let Some(window) = Window::from_id(&id) else { continue };
                        if left(window, *missions) || matches!(window, Window::Inventory(inventory) if !inventory_granted || !inventories.contains(inventory)) {
                            continue;
                        }
                        window_retries.loaded(window);
                        pending_reports.cancel(window);
                        let rows = windows.entry(window).or_default();
                        for id in removes {
                            rows.remove(&id);
                        }
                        rows.extend(upserts.into_iter().map(|item| (item.header().id.clone(), item)));
                        if !send_window(updates, received, window, snapshot, rows, &order, has_more) {
                            return Ended::Closed;
                        }
                    }
                    // st keeps a conversation's subscription and retries it itself; it says why
                    // so the last copy shown can say it is stale (the owner's host is away).
                    CollectionEvent::Resync { id, code, message: Some(message) } if id.starts_with(CONVERSATION) => {
                        let Some(current) = conversing.iter().find(|current| current.id == id) else { continue };
                        let message = st3_client::plain_message(code.as_ref(), &message);
                        if updates.send(Update::ConversationFailed { target: current.target.clone(), code, message, permanent: false }).is_err() {
                            return Ended::Closed;
                        }
                    }
                    // st asks for a window again (its cursor fell behind a floor, or the graph moved
                    // under it). Asked at once and again on every resync, that is a loop and, at an
                    // upgrade, a herd; so each is asked after a wait that grows and is jittered, and
                    // one good snapshot resets the wait.
                    CollectionEvent::Resync { id, code, message } => {
                        if let Some(window) = Window::from_id(&id)
                            && !(left(window, *missions) || matches!(window, Window::Inventory(inventory) if !inventory_granted || !inventories.contains(inventory)))
                        {
                            if matches!(window, Window::Inventory(_)) {
                                if let Some(message) = message {
                                    let plain = st3_client::plain_message(code.as_ref(), &message);
                                    pending_reports.report(window, Instant::now(), plain);
                                }
                                continue;
                            }
                            // Said once, with st's own reason when it gave one (its source is
                            // not ready); the retries are quiet until the window loads.
                            let first = window_retries.failed(window, Instant::now());
                            if first && let Some(message) = message {
                                let message = st3_client::plain_message(code.as_ref(), &message);
                                pending_reports.report(window, Instant::now(), message);
                            }
                        }
                    }
                    CollectionEvent::Conversation { id, session_id, replace, items, has_more, header } => {
                        let Some(current) = conversing.iter_mut().find(|current| current.id == id) else { continue };
                        current.failures = 0;
                        if updates.send(Update::Conversation { target: current.target.clone(), session_id, replace, has_more, items, header }).is_err() {
                            return Ended::Closed;
                        }
                    }
                    CollectionEvent::Error { id, code, message } if id.starts_with(CONVERSATION) => {
                        let message = st3_client::plain_message(code.as_ref(), &message);
                        let Some(current) = conversing.iter_mut().find(|current| current.id == id) else { continue };
                        let permanent = !conversation_may_clear(code.as_ref());
                        // Anything else may clear: the agent starts, its host comes back, its
                        // history arrives. Ask again after a backoff.
                        if !permanent {
                            current.retry_at = Some(Instant::now() + RETRY_DELAYS[current.failures.min(RETRY_DELAYS.len() - 1)]);
                            current.failures += 1;
                        }
                        if updates.send(Update::ConversationFailed { target: current.target.clone(), code, message, permanent }).is_err() {
                            return Ended::Closed;
                        }
                    }
                    CollectionEvent::Error { id, code, message } => {
                        if let Some(window) = Window::from_id(&id) {
                            if left(window, *missions) || matches!(window, Window::Inventory(inventory) if !inventory_granted || !inventories.contains(inventory)) {
                                continue;
                            }
                            if matches!(window, Window::Inventory(_)) {
                                let plain = st3_client::plain_message(code.as_ref(), &message);
                                pending_reports.report(window, Instant::now(), plain);
                                continue;
                            }
                            // Said once, and only if it lasts; the retries are quiet until it loads.
                            if window_retries.failed(window, Instant::now()) {
                                let message = st3_client::plain_message(code.as_ref(), &message);
                                pending_reports.report(window, Instant::now(), message);
                            }
                        } else if id == TERMINAL {
                            terminal_failed(updates, following, code, message);
                        }
                    }
                    CollectionEvent::Screen { id, screen } => {
                        if id != TERMINAL {
                            continue;
                        }
                        if let Some(current) = following.as_mut() {
                            current.failures = 0;
                        }
                        if updates.send(Update::Terminal(TerminalUpdate::Screen(Box::new(screen.value)))).is_err() {
                            return Ended::Closed;
                        }
                    }
                }
            }
            command = commands.recv() => match command {
                Some(Command::Reconnect) => {}
                None => {
                    stop_following(client, stream, following).await;
                    return Ended::Closed;
                }
                Some(Command::Unfollow) => stop_following(client, stream, following).await,
                Some(Command::Missions { follow }) => {
                    if follow == *missions {
                        continue;
                    }
                    *missions = follow;
                    if follow {
                        if let Err(error) = stream.subscribe(Window::Missions.id(), Window::Missions.id(), Window::Missions.limit(), None, None).await {
                            return Ended::Dropped(error.to_string());
                        }
                    } else {
                        let _ = stream.unsubscribe(Window::Missions.id()).await;
                        windows.remove(&Window::Missions);
                        window_retries.loaded(Window::Missions);
                        pending_reports.cancel(Window::Missions);
                        if updates.send(Update::WindowStopped(Window::Missions)).is_err() {
                            return Ended::Closed;
                        }
                    }
                }
                Some(Command::Inventories(wanted)) => {
                    let previous = *inventories;
                    *inventories = wanted;
                    if !inventory_granted { continue; }
                    for inventory in previous.windows().into_iter().filter(|old| !wanted.contains(*old)) {
                        if let Err(error) = stream.unsubscribe(&inventory.id()).await { return Ended::Dropped(error.plain()); }
                        let window = Window::Inventory(inventory);
                        windows.remove(&window); window_retries.loaded(window);
                        pending_reports.cancel(window);
                        if updates.send(Update::WindowStopped(window)).is_err() { return Ended::Closed; }
                    }
                    for inventory in wanted.windows().into_iter().filter(|new| !previous.contains(*new)) {
                        if let Err(error) = inventory.subscribe(stream).await { return Ended::Dropped(error.plain()); }
                    }
                }
                Some(Command::Converse { targets }) => {
                    let targets = targets.into_iter().take(MAX_CONVERSATIONS).collect::<Vec<_>>();
                    // Leave what is no longer shown; keep what still is, subscribed as it is.
                    let mut kept = Vec::new();
                    for current in std::mem::take(conversing) {
                        if targets.contains(&current.target) {
                            kept.push(current);
                        } else {
                            let _ = stream.unsubscribe(&current.id).await;
                        }
                    }
                    for target in targets {
                        if kept.iter().any(|current| current.target == target) {
                            continue;
                        }
                        let mut current = Conversing::new(target);
                        if let Err(error) = converse(stream, &mut current).await {
                            return Ended::Dropped(error.to_string());
                        }
                        kept.push(current);
                    }
                    *conversing = kept;
                }
                Some(Command::Resubscribe { target }) => {
                    if let Some(current) = conversing.iter_mut().find(|current| current.target == target) {
                        let _ = stream.unsubscribe(&current.id).await;
                        current.retry_at = None;
                        current.failures = 0;
                        if let Err(error) = converse(stream, current).await {
                            return Ended::Dropped(error.to_string());
                        }
                    }
                }
                Some(Command::Follow { runtime_ids }) => {
                    stop_following(client, stream, following).await;
                    match resolve(client, &runtime_ids).await {
                        Ok((runtime_id, terminal_id)) => {
                            *following = Some(Following {
                                runtime_id, terminal_id, incarnation: None, attachment_id: None, retry_at: None, failures: 0,
                            });
                            if let Err(error) = follow(client, stream, updates, following).await {
                                return Ended::Dropped(error.to_string());
                            }
                        }
                        Err(reason) => {
                            let _ = updates.send(Update::Terminal(TerminalUpdate::Ended { restarted: false, reason }));
                        }
                    }
                }
            },
            () = tokio::time::sleep_until(retry_at.unwrap_or_else(Instant::now)), if retry_at.is_some() => {
                if let Err(error) = follow(client, stream, updates, following).await {
                    return Ended::Dropped(error.to_string());
                }
            }
            () = tokio::time::sleep_until(report_at.unwrap_or_else(Instant::now)), if report_at.is_some() => {
                let now = Instant::now();
                for (window, message) in pending_reports.due(now) {
                    if updates.send(Update::WindowFailed(window, message)).is_err() {
                        return Ended::Closed;
                    }
                }
            }
            () = tokio::time::sleep_until(window_at.unwrap_or_else(Instant::now)), if window_at.is_some() => {
                for window in window_retries.due(Instant::now()) {
                    if window == Window::Missions && !*missions {
                        continue;
                    }
                    let result = if window == Window::Arrangements {
                        match sidebar_person {
                            Some(person) => stream.subscribe_arrangements(window.id(), person, window.limit(), None).await,
                            None => continue,
                        }
                    } else {
                        stream.subscribe(window.id(), window.id(), window.limit(), None, None).await
                    };
                    if let Err(error) = result {
                        return Ended::Dropped(error.to_string());
                    }
                }
            }
            () = tokio::time::sleep_until(converse_at.unwrap_or_else(Instant::now)), if converse_at.is_some() => {
                let now = Instant::now();
                for current in conversing.iter_mut().filter(|current| current.retry_at.is_some_and(|at| at <= now)) {
                    if let Err(error) = converse(stream, current).await {
                        return Ended::Dropped(error.to_string());
                    }
                }
            }
        }
    }
}

/// Whether a frame for `window` is one already on its way when the window was left: the missions
/// window is followed only on request, and a snapshot, change, resync or error that arrives after
/// the unsubscribe must not bring back the rows, the followed state or an error the UI dropped.
fn left(window: Window, missions_followed: bool) -> bool {
    window == Window::Missions && !missions_followed
}

/// Up to half of `wait` more, so many clients told to ask again at once do not all ask together.
fn jitter(wait: Duration) -> Duration {
    let spread = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| u64::from(since.subsec_nanos()) % 1000);
    wait.mul_f64(spread as f64 / 2000.0)
}

/// Why a window is not being served, held until it has lasted `REPORT_AFTER`: a window that
/// loads (or is left) in that time is never reported, so a failure that mends by itself says
/// nothing, and one that lasts is said once.
#[derive(Default)]
struct PendingReports {
    reports: Vec<(Window, Instant, String)>,
}

impl PendingReports {
    /// `window` failed at `now` for `message`; keeps the first reason while one is pending.
    fn report(&mut self, window: Window, now: Instant, message: String) {
        if !self.reports.iter().any(|(pending, _, _)| *pending == window) {
            self.reports.push((window, now + REPORT_AFTER, message));
        }
    }

    /// The window loaded or was left: whatever was pending for it is moot.
    fn cancel(&mut self, window: Window) {
        self.reports.retain(|(pending, _, _)| *pending != window);
    }

    /// When the next report falls due.
    fn next(&self) -> Option<Instant> {
        self.reports.iter().map(|(_, at, _)| *at).min()
    }

    /// The reports due by `now`, each taken off.
    fn due(&mut self, now: Instant) -> Vec<(Window, String)> {
        let mut due = Vec::new();
        self.reports.retain(|(window, at, message)| {
            if *at <= now {
                due.push((*window, message.clone()));
                false
            } else {
                true
            }
        });
        due
    }
}

/// Windows st stopped sending, and when to ask for each again.
#[derive(Default)]
struct WindowRetries {
    failures: BTreeMap<Window, usize>,
    at: BTreeMap<Window, Instant>,
}

impl WindowRetries {
    /// `window` failed: ask again after a wait that grows with each failure. Whether this is
    /// its first failure since it last loaded, the one worth telling the person about.
    fn failed(&mut self, window: Window, now: Instant) -> bool {
        let failures = self.failures.entry(window).or_default();
        let wait = RETRY_DELAYS[(*failures).min(RETRY_DELAYS.len() - 1)];
        self.at.insert(window, now + wait + jitter(wait));
        *failures += 1;
        *failures == 1
    }

    fn loaded(&mut self, window: Window) {
        self.failures.remove(&window);
        self.at.remove(&window);
    }

    /// When the next window is due.
    fn next(&self) -> Option<Instant> {
        self.at.values().min().copied()
    }

    /// The windows due by `now`, each taken off until it fails again.
    fn due(&mut self, now: Instant) -> Vec<Window> {
        let due = self
            .at
            .iter()
            .filter(|(_, at)| **at <= now)
            .map(|(window, _)| *window)
            .collect::<Vec<_>>();
        for window in &due {
            self.at.remove(window);
        }
        due
    }
}

fn send_window(
    updates: &mpsc::Sender<Update>,
    received: std::time::Instant,
    window: Window,
    snapshot: Snapshot,
    rows: &BTreeMap<String, Resource>,
    order: &[String],
    has_more: bool,
) -> bool {
    let items = order
        .iter()
        .filter_map(|id| rows.get(id).cloned())
        .collect();
    updates
        .send(Update::Window {
            received,
            window,
            snapshot,
            items,
            has_more,
        })
        .is_ok()
}

/// A terminal subscription ended with an error. A terminal out of reach attaches again after a
/// backoff; one whose process exited (`terminal-ended`), or any other refusal, stops following.
fn terminal_failed(
    updates: &mpsc::Sender<Update>,
    following: &mut Option<Following>,
    code: Option<ErrorCode>,
    message: String,
) {
    let Some(current) = following.as_mut() else {
        return;
    };
    current.attachment_id = None;
    let plain = terminal_reason(code.as_ref(), &message);
    let update = match code {
        // A daemon from before terminal-unavailable also says stale-fence when the owner is
        // briefly out of reach or the viewer idled: follow again, and the attach itself refuses a
        // terminal that really restarted.
        Some(
            ErrorCode::StaleFence
            | ErrorCode::TerminalUnavailable
            | ErrorCode::Internal
            | ErrorCode::RemoteUnavailable
            | ErrorCode::RateLimited
            | ErrorCode::RuntimeAuthorityIndeterminate,
        ) => {
            current.retry_at =
                Some(Instant::now() + RETRY_DELAYS[current.failures.min(RETRY_DELAYS.len() - 1)]);
            current.failures += 1;
            TerminalUpdate::Reconnecting(plain)
        }
        _ => {
            *following = None;
            TerminalUpdate::Ended {
                restarted: false,
                reason: plain,
            }
        }
    };
    let _ = updates.send(Update::Terminal(update));
}

/// Attach to the followed terminal and subscribe to it on this socket. A refused attach ends
/// following; only a failure to write to the socket is returned.
async fn follow(
    client: &Client,
    stream: &mut CollectionStream,
    updates: &mpsc::Sender<Update>,
    following: &mut Option<Following>,
) -> Result<(), ClientError> {
    let Some(current) = following.as_mut() else {
        return Ok(());
    };
    current.retry_at = None;
    let attachment = match attach(
        client,
        &current.runtime_id,
        &current.terminal_id,
        current.incarnation.as_deref(),
    )
    .await
    {
        Ok(attachment) => attachment,
        Err(Refusal::Restarted) => {
            *following = None;
            let _ = updates.send(Update::Terminal(TerminalUpdate::Ended {
                restarted: true,
                reason: "Terminal restarted".into(),
            }));
            return Ok(());
        }
        Err(Refusal::Unavailable(reason)) => {
            current.retry_at =
                Some(Instant::now() + RETRY_DELAYS[current.failures.min(RETRY_DELAYS.len() - 1)]);
            current.failures += 1;
            let _ = updates.send(Update::Terminal(TerminalUpdate::Reconnecting(reason)));
            return Ok(());
        }
        Err(Refusal::Failed(reason)) => {
            *following = None;
            let _ = updates.send(Update::Terminal(TerminalUpdate::Ended {
                restarted: false,
                reason,
            }));
            return Ok(());
        }
    };
    let Some(capability) = attachment.stream_capability.clone() else {
        *following = None;
        let _ = updates.send(Update::Terminal(TerminalUpdate::Ended {
            restarted: false,
            reason: "st returned no stream capability for the terminal".into(),
        }));
        return Ok(());
    };
    current.incarnation = Some(attachment.runtime_incarnation.clone());
    current.attachment_id = Some(attachment.attachment_id.clone());
    let _ = updates.send(Update::Terminal(TerminalUpdate::Attached {
        terminal_id: current.terminal_id.clone(),
        attachment_id: attachment.attachment_id,
        incarnation: attachment.runtime_incarnation.clone(),
    }));
    stream
        .subscribe_terminal(
            TERMINAL,
            &current.terminal_id,
            Some(&attachment.runtime_incarnation),
            &capability,
        )
        .await
}

impl Conversing {
    fn new(target: String) -> Self {
        Self {
            id: format!("{CONVERSATION}:{}", uuid::Uuid::now_v7()),
            target,
            retry_at: None,
            failures: 0,
        }
    }
}

/// Subscribe to a shown conversation on this socket; a held subscription is replaced.
async fn converse(
    stream: &mut CollectionStream,
    current: &mut Conversing,
) -> Result<(), ClientError> {
    current.retry_at = None;
    stream
        .subscribe_conversation(&current.id, &current.target)
        .await
}

/// Stop following: leave the subscription and end the viewer record.
async fn stop_following(
    client: &Client,
    stream: &mut CollectionStream,
    following: &mut Option<Following>,
) {
    let Some(current) = following.take() else {
        return;
    };
    let _ = stream.unsubscribe(TERMINAL).await;
    if let (Some(attachment_id), Some(incarnation)) = (current.attachment_id, current.incarnation) {
        let client = client.clone();
        tokio::spawn(async move {
            let _ = detach(&client, &current.terminal_id, &attachment_id, &incarnation).await;
        });
    }
}

enum Refusal {
    /// The runtime now runs another incarnation, or none.
    Restarted,
    /// Out of reach for now (its host, or which host runs it): try again in a while.
    Unavailable(String),
    Failed(String),
}

/// Why a terminal request failed, in words. st says stale-fence for an owner out of reach too, and
/// then its own words say so better than the generic sentence.
fn terminal_reason(code: Option<&ErrorCode>, message: &str) -> String {
    match code {
        Some(ErrorCode::StaleFence) if !message.is_empty() => message.to_owned(),
        _ => st3_client::plain_message(code, message),
    }
}

/// A failed terminal request as a refusal: one that may pass on its own is tried again later.
fn refusal(error: ClientError) -> Refusal {
    let reason = match &error {
        ClientError::Api(code, message, _) => terminal_reason(Some(code), message),
        other => other.plain(),
    };
    if error.is_transient() {
        Refusal::Unavailable(reason)
    } else {
        Refusal::Failed(reason)
    }
}

/// The first of these runtimes that has a terminal.
async fn resolve(client: &Client, runtime_ids: &[String]) -> Result<(String, String), String> {
    for id in runtime_ids {
        if let Ok(envelope) = client.runtimes_get(id).await
            && let Resource::Runtime(runtime) = envelope.value
            && let Some(terminal) = runtime.terminal_id
        {
            return Ok((runtime.header.id, terminal));
        }
    }
    Err("that agent has no terminal right now".into())
}

/// Whether a refused conversation may load if asked again. Only st's word that it keeps no
/// start for the transcript (`timeline-history-incomplete`) is final: asking again cannot help.
fn conversation_may_clear(code: Option<&ErrorCode>) -> bool {
    !matches!(code, Some(ErrorCode::TimelineHistoryIncomplete))
}

/// How many times a fenced terminal request is tried while it races a busy store.
const FENCE_TRIES: u32 = 8;

/// `terminal.attach` with a fresh fence, retried on `stale-fence` with a growing pause. With `expected`,
/// a runtime now on another incarnation is a restart rather than something to attach to.
async fn attach(
    client: &Client,
    runtime_id: &str,
    terminal_id: &str,
    expected: Option<&str>,
) -> Result<st3_client::TerminalAttachment, Refusal> {
    for attempt in 0..FENCE_TRIES {
        let current = client.runtimes_get(runtime_id).await.map_err(refusal)?;
        let Resource::Runtime(runtime) = current.value else {
            return Err(Refusal::Failed("the runtime is no longer available".into()));
        };
        if runtime.terminal_id.as_deref() != Some(terminal_id) {
            return Err(Refusal::Restarted);
        }
        if let Some(expected) = expected
            && runtime.incarnation_id.as_deref() != Some(expected)
        {
            return Err(Refusal::Restarted);
        }
        let fence = Fence {
            snapshot_id: current.snapshot.id,
            runtime_incarnation: runtime.incarnation_id,
            terminal_sequence: runtime.terminal_sequence,
            ..Fence::default()
        };
        let (id, key) = crate::action_pair();
        match client
            .terminal_attach(
                id,
                key,
                fence,
                TargetParameters {
                    target_id: terminal_id.to_owned(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(response) => {
                return response
                    .value
                    .terminal_attachment
                    .ok_or_else(|| Refusal::Failed("st attached no viewer".into()));
            }
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt + 1 < FENCE_TRIES => {
                tokio::time::sleep(Duration::from_millis(25 << attempt)).await;
                continue;
            }
            Err(error) => return Err(refusal(error)),
        }
    }
    Err(Refusal::Unavailable(
        "the terminal kept changing while attaching".into(),
    ))
}

/// End a viewer record with a fresh fence, retried on `stale-fence` with a growing pause.
pub async fn detach(
    client: &Client,
    terminal_id: &str,
    attachment_id: &str,
    incarnation: &str,
) -> anyhow::Result<()> {
    for attempt in 0..FENCE_TRIES {
        let fence = crate::terminal_fence(client, terminal_id, incarnation).await?;
        let (id, key) = crate::action_pair();
        match client
            .terminal_detach(
                id,
                key,
                fence,
                TargetParameters {
                    target_id: attachment_id.to_owned(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt + 1 < FENCE_TRIES => {
                tokio::time::sleep(Duration::from_millis(25 << attempt)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_failure_is_said_only_once_it_has_lasted_and_a_load_cancels_it() {
        let start = Instant::now();
        let mut reports = PendingReports::default();
        reports.report(Window::Summary, start, "st is catching up".into());
        // A second failure of the same window keeps the first reason and its clock.
        reports.report(Window::Summary, start + REPORT_AFTER / 2, "later reason".into());
        assert_eq!(reports.next(), Some(start + REPORT_AFTER));
        // Quiet while it is young, up to the last instant before the delay.
        assert!(reports.due(start).is_empty());
        assert!(reports.due(start + REPORT_AFTER - Duration::from_millis(1)).is_empty());
        // Said, once, when it has lasted.
        assert_eq!(
            reports.due(start + REPORT_AFTER),
            [(Window::Summary, "st is catching up".to_owned())]
        );
        assert!(reports.due(start + REPORT_AFTER * 2).is_empty());
        assert_eq!(reports.next(), None);

        // A window that loads before the delay is never reported, and others are left alone.
        reports.report(Window::Arrangements, start, "a".into());
        reports.report(Window::Inventory(Inventory::Machines), start, "b".into());
        reports.cancel(Window::Arrangements);
        assert_eq!(
            reports.due(start + REPORT_AFTER),
            [(Window::Inventory(Inventory::Machines), "b".to_owned())]
        );
        // The same window can fail and be reported again after it has loaded in between.
        reports.report(Window::Arrangements, start + REPORT_AFTER, "again".into());
        assert_eq!(reports.next(), Some(start + REPORT_AFTER * 2));
    }


    #[test]
    fn conversation_ids_are_bounded_and_distinct_for_long_targets() {
        let target = format!("session/{}", "descriptive-child-task".repeat(256));
        let first = Conversing::new(target.clone());
        let same_target = Conversing::new(target.clone());
        let other_target = Conversing::new(format!("{target}-other"));
        for current in [&first, &same_target, &other_target] {
            assert!(current.id.len() <= 128);
            let suffix = current.id.strip_prefix("conversation:").unwrap();
            let uuid = uuid::Uuid::parse_str(suffix).unwrap();
            assert_eq!(uuid.get_version_num(), 7);
        }
        assert_eq!(first.target, target);
        assert_eq!(same_target.target, target);
        assert_ne!(first.id, same_target.id);
        assert_ne!(first.id, other_target.id);
        assert_ne!(same_target.id, other_target.id);
    }

    fn following() -> Option<Following> {
        Some(Following {
            runtime_id: "runtime/demo".into(),
            terminal_id: "terminal/demo".into(),
            incarnation: Some("demo:i1".into()),
            attachment_id: Some("terminal-attachment/demo".into()),
            retry_at: None,
            failures: 0,
        })
    }

    fn terminal_update(updates: &mpsc::Receiver<Update>) -> TerminalUpdate {
        match updates.try_recv().unwrap() {
            Update::Terminal(update) => update,
            other => panic!("expected a terminal update, got {other:?}"),
        }
    }

    #[test]
    fn a_window_st_stopped_is_asked_for_again_after_a_growing_wait() {
        let mut retries = WindowRetries::default();
        let start = Instant::now();
        assert!(
            retries.failed(Window::Agents, start),
            "the first failure is said"
        );
        // The wait is the step's, plus up to half of it so clients do not all ask together.
        let first = retries.next().unwrap();
        assert!(first >= start + RETRY_DELAYS[0] && first <= start + RETRY_DELAYS[0] * 3 / 2);
        assert!(retries.due(start).is_empty());
        assert_eq!(retries.due(start + RETRY_DELAYS[0] * 2), vec![Window::Agents]);
        assert_eq!(retries.next(), None, "asked again; it waits for an answer");
        assert!(
            !retries.failed(Window::Agents, start),
            "later failures are quiet"
        );
        let second = retries.next().unwrap();
        assert!(second >= start + RETRY_DELAYS[1] && second <= start + RETRY_DELAYS[1] * 3 / 2);
        retries.loaded(Window::Agents);
        assert_eq!(retries.next(), None);
        assert!(
            retries.failed(Window::Agents, start),
            "after loading, a failure is said again"
        );
    }

    fn api_error(code: ErrorCode, message: &str) -> ClientError {
        ClientError::Api(
            code.clone(),
            message.into(),
            Box::new(st3_client::ErrorEnvelope {
                api_version: "v0".into(),
                error_version: "1".into(),
                request_id: "request".into(),
                code,
                message: message.into(),
                retryable: false,
                retry_after_ms: None,
                details: Default::default(),
            }),
        )
    }

    #[test]
    fn a_window_st_asks_for_again_waits_longer_each_time_and_a_snapshot_resets_it() {
        let mut retries = WindowRetries::default();
        let now = Instant::now();
        let mut last = Duration::ZERO;
        for (index, base) in RETRY_DELAYS.iter().enumerate() {
            retries.failed(Window::Missions, now);
            let wait = retries.next().unwrap() - now;
            assert!(
                wait >= *base && wait <= *base + *base / 2,
                "try {index}: {wait:?} for a base of {base:?}"
            );
            assert!(wait >= last, "the wait only grows");
            last = *base;
            // Nothing is asked again before it is due.
            assert!(retries.due(now).is_empty());
        }
        retries.loaded(Window::Missions);
        assert!(retries.next().is_none(), "a snapshot clears the wait");
        retries.failed(Window::Missions, now);
        assert!(retries.next().unwrap() - now < RETRY_DELAYS[0] * 2, "and starts it over");
    }

    #[test]
    fn only_incomplete_history_stops_a_conversation_retrying() {
        assert!(!conversation_may_clear(Some(
            &ErrorCode::TimelineHistoryIncomplete
        )));
        for code in [
            ErrorCode::CursorGap,
            ErrorCode::StaleFence,
            ErrorCode::NotFound,
        ] {
            assert!(conversation_may_clear(Some(&code)), "{code:?}");
        }
        assert!(conversation_may_clear(None));
    }

    #[test]
    fn an_attach_its_owner_cannot_answer_is_tried_again_in_words() {
        // Nathan, 2026-10-02: attaching to a terminal on another member ended at once with
        // "st client API error StaleFence: …". It is out of reach for now, not over.
        let refused = refusal(api_error(
            ErrorCode::StaleFence,
            "the terminal owner is not reachable",
        ));
        assert!(
            matches!(&refused, Refusal::Unavailable(reason) if reason == "the terminal owner is not reachable")
        );
        let refused = refusal(api_error(
            ErrorCode::RuntimeAuthorityIndeterminate,
            "subject `agent/x` has indeterminate runtime authority",
        ));
        assert!(
            matches!(&refused, Refusal::Unavailable(reason) if reason == "st cannot tell yet which host runs this")
        );
        let refused = refusal(api_error(
            ErrorCode::Forbidden,
            "only the person may attach",
        ));
        assert!(matches!(refused, Refusal::Failed(reason) if reason.starts_with("not allowed")));
    }

    #[test]
    fn stale_fence_on_the_stream_follows_again_and_the_attach_decides_if_it_restarted() {
        // st says stale-fence for an owner briefly out of reach or a viewer that idled, not only
        // for a restart: stui follows again, and `attach` refuses a changed incarnation.
        let (tx, rx) = mpsc::channel();
        let mut current = following();
        terminal_failed(
            &tx,
            &mut current,
            Some(ErrorCode::StaleFence),
            "the terminal owner is not reachable".into(),
        );
        assert!(
            current
                .as_ref()
                .is_some_and(|state| state.retry_at.is_some())
        );
        assert!(matches!(
            terminal_update(&rx),
            TerminalUpdate::Reconnecting(reason) if !reason.contains("StaleFence")
        ));
    }

    #[test]
    fn a_transient_failure_reattaches_after_a_growing_wait_and_a_refusal_stops() {
        let (tx, rx) = mpsc::channel();
        let mut current = following();
        for (attempt, delay) in RETRY_DELAYS.iter().take(3).enumerate() {
            let before = Instant::now();
            terminal_failed(
                &tx,
                &mut current,
                Some(ErrorCode::RemoteUnavailable),
                "owner away".into(),
            );
            let state = current
                .as_ref()
                .expect("a transient failure keeps following");
            assert_eq!(state.failures, attempt + 1);
            assert!(state.attachment_id.is_none(), "the old attachment is spent");
            let wait = state.retry_at.unwrap() - before;
            assert!(
                wait >= *delay && wait < *delay + Duration::from_secs(1),
                "{wait:?}"
            );
            assert!(matches!(
                terminal_update(&rx),
                TerminalUpdate::Reconnecting(_)
            ));
        }
        terminal_failed(&tx, &mut current, Some(ErrorCode::Forbidden), "no".into());
        assert!(current.is_none(), "a refusal is not retried");
        assert!(matches!(
            terminal_update(&rx),
            TerminalUpdate::Ended { restarted: false, reason } if reason == "not allowed: no"
        ));
    }

    #[test]
    fn a_frame_for_the_missions_window_after_it_was_left_is_ignored() {
        // The other windows are never left; missions are ignored only while not followed.
        for window in [Window::Attention, Window::Agents, Window::Glasses, Window::Summary] {
            assert!(!left(window, false), "{window:?} is always followed");
            assert!(!left(window, true));
        }
        assert!(left(Window::Missions, false), "a late missions frame is dropped");
        assert!(!left(Window::Missions, true), "followed missions are kept");
    }

}

