//! One socket to st for live collection windows, conversations and a terminal.
//!
//! The feed holds one collection socket with the attention, missions and agents windows, the
//! open conversations, and, while a terminal is open, that terminal. st joins each row and each
//! conversation, so callers never join lists themselves or read item by item. When the socket drops,
//! the feed opens a new one, subscribes again, and attaches the open terminal again. Nothing
//! here polls projections while connected: frames arrive only when something changed. Remote
//! devices also make a bounded liveness read so an idle network blackhole becomes offline.

#![doc = include_str!("../README.md")]

pub mod cache;
pub mod model;
mod terminal;
pub mod tree;

pub use terminal::{action_pair, terminal_fence, terminal_screen_fence};

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

/// The waits between attempts to reach st again, reset once st answers.
const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

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
}

impl Window {
    const ALL: [Self; 3] = [Self::Attention, Self::Missions, Self::Agents];

    fn id(self) -> &'static str {
        match self {
            Self::Attention => "attention",
            Self::Missions => "missions",
            Self::Agents => "agents",
            Self::Glasses => "glasses",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .chain([Self::Glasses])
            .find(|window| window.id() == id)
    }

    /// How many items to follow: every glass a person may keep, a page of anything else.
    fn limit(self) -> usize {
        match self {
            Self::Glasses => GLASSES,
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
    /// The granted glasses shape, delivered before its window. The embedding UI chooses how
    /// to store glasses for this version; the feed owns no application state.
    GlassesVersion(u32),
    /// A window's current items, in st's display order.
    Window {
        window: Window,
        snapshot: Snapshot,
        items: Vec<Resource>,
        has_more: bool,
    },
    /// st refused a window; its last items stay.
    WindowFailed(Window, String),
    Terminal(TerminalUpdate),
    /// The open conversation's entries. `replace` means these are its newest page and every
    /// earlier entry is gone; otherwise they are new or revised entries, matched by ID.
    Conversation {
        target: String,
        /// The session the entries belong to: earlier pages are read from its timeline.
        session_id: String,
        replace: bool,
        has_more: bool,
        items: Vec<TimelineEntry>,
    },
    /// st could not show the open conversation; the feed asks again after a backoff, unless
    /// st said it never can (`permanent`).
    ConversationFailed {
        target: String,
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

/// The most conversations followed at once. A socket holds eight subscriptions: three windows,
/// glasses and a terminal leave three.
pub const MAX_CONVERSATIONS: usize = 3;

/// A conversation being shown, on its own subscription.
struct Conversing {
    target: String,
    /// The subscription id, named after the target so a frame never lands on another one.
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
    mut commands: channel::UnboundedReceiver<Command>,
) {
    if clients.is_empty() {
        return;
    }
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
                &mut stream,
                &updates,
                &mut commands,
                &mut following,
                &mut conversing,
                &mut failures,
            )
            .await
            {
                Ended::Closed => return,
                Ended::Dropped(reason) => {
                    if updates.send(Update::Offline(reason.clone())).is_err() {
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
        } else if updates.send(Update::Offline(reason)).is_err() {
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
    stream: &mut CollectionStream,
    updates: &mpsc::Sender<Update>,
    commands: &mut channel::UnboundedReceiver<Command>,
    following: &mut Option<Following>,
    conversing: &mut Vec<Conversing>,
    failures: &mut usize,
) -> Ended {
    // A blackholed network, or a wedged daemon, can leave a socket open and silent while the
    // views say live. A bounded read every 10 s notices, on this host too; one slow answer from
    // a busy daemon is not enough to drop everything: it is asked again at once, and only two
    // misses in a row drop the socket. It never resends a mutation.
    let mut probe = tokio::time::interval(Duration::from_secs(10));
    probe.tick().await;
    // Glasses are followed only where st grants them in the shape this stui reads (splits of
    // tab groups, version 1); elsewhere stui keeps them on the device. A member still on the
    // earlier shape would send glasses this stui cannot decode, and that drops the connection.
    let version = if glasses {
        client.capabilities().await.ok().and_then(|capabilities| {
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
        })
    } else {
        None
    };
    // From version 2 a glass's splits keep their sizes in st.
    if let Some(version) = version
        && updates.send(Update::GlassesVersion(version)).is_err()
    {
        return Ended::Closed;
    }
    let granted = version.is_some();
    for window in Window::ALL
        .into_iter()
        .chain(granted.then_some(Window::Glasses))
    {
        if let Err(error) = stream
            .subscribe(window.id(), window.id(), window.limit(), None, None)
            .await
        {
            return Ended::Dropped(error.to_string());
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
        tokio::select! {
            _ = probe.tick() => {
                let mut answered = false;
                for _ in 0..2 {
                    match tokio::time::timeout(Duration::from_secs(5), client.capabilities()).await {
                        Ok(Ok(_)) => {
                            answered = true;
                            break;
                        }
                        Ok(Err(error)) if !error.is_transient() => return Ended::Dropped(error.plain()),
                        Ok(Err(_)) | Err(_) => {}
                    }
                }
                if !answered {
                    return Ended::Dropped(if remote {
                        "the member stopped answering".into()
                    } else {
                        "st stopped answering".into()
                    });
                }
                *failures = 0;
            }
            event = stream.next_event() => {
                let event = match event {
                    Ok(Some(event)) => event,
                    Ok(None) => return Ended::Dropped("st closed the connection".into()),
                    Err(error) => return Ended::Dropped(error.to_string()),
                };
                *failures = 0;
                match event {
                    CollectionEvent::Snapshot { id, snapshot, items, order, has_more } => {
                        let Some(window) = Window::from_id(&id) else { continue };
                        *failures = 0;
                        window_retries.loaded(window);
                        let rows = windows.entry(window).or_default();
                        rows.clear();
                        rows.extend(items.into_iter().map(|item| (item.header().id.clone(), item)));
                        if !send_window(updates, window, snapshot, rows, &order, has_more) {
                            return Ended::Closed;
                        }
                    }
                    CollectionEvent::Changes { id, snapshot, upserts, removes, order, has_more } => {
                        let Some(window) = Window::from_id(&id) else { continue };
                        let rows = windows.entry(window).or_default();
                        for id in removes {
                            rows.remove(&id);
                        }
                        rows.extend(upserts.into_iter().map(|item| (item.header().id.clone(), item)));
                        if !send_window(updates, window, snapshot, rows, &order, has_more) {
                            return Ended::Closed;
                        }
                    }
                    // st keeps a conversation's subscription and retries it itself; it says why
                    // so the last copy shown can say it is stale (the owner's host is away).
                    CollectionEvent::Resync { id, code, message: Some(message) } if id.starts_with(CONVERSATION) => {
                        let Some(current) = conversing.iter().find(|current| current.id == id) else { continue };
                        let message = st3_client::plain_message(code.as_ref(), &message);
                        if updates.send(Update::ConversationFailed { target: current.target.clone(), message, permanent: false }).is_err() {
                            return Ended::Closed;
                        }
                    }
                    CollectionEvent::Resync { id, .. } => {
                        if let Some(window) = Window::from_id(&id)
                            && let Err(error) = stream.subscribe(window.id(), window.id(), window.limit(), None, None).await
                        {
                            return Ended::Dropped(error.to_string());
                        }
                    }
                    CollectionEvent::Conversation { id, session_id, replace, items, has_more } => {
                        let Some(current) = conversing.iter_mut().find(|current| current.id == id) else { continue };
                        current.failures = 0;
                        if updates.send(Update::Conversation { target: current.target.clone(), session_id, replace, has_more, items }).is_err() {
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
                        if updates.send(Update::ConversationFailed { target: current.target.clone(), message, permanent }).is_err() {
                            return Ended::Closed;
                        }
                    }
                    CollectionEvent::Error { id, code, message } => {
                        if let Some(window) = Window::from_id(&id) {
                            // Said once; the retries are quiet until it loads.
                            if window_retries.failed(window, Instant::now()) {
                                let message = st3_client::plain_message(code.as_ref(), &message);
                                if updates.send(Update::WindowFailed(window, message)).is_err() {
                                    return Ended::Closed;
                                }
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
            () = tokio::time::sleep_until(window_at.unwrap_or_else(Instant::now)), if window_at.is_some() => {
                for window in window_retries.due(Instant::now()) {
                    if let Err(error) = stream.subscribe(window.id(), window.id(), window.limit(), None, None).await {
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
        self.at.insert(
            window,
            now + RETRY_DELAYS[(*failures).min(RETRY_DELAYS.len() - 1)],
        );
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
            id: format!("{CONVERSATION}:{target}"),
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
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::{Notify, watch};

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
        assert_eq!(retries.next(), Some(start + RETRY_DELAYS[0]));
        assert!(retries.due(start).is_empty());
        assert_eq!(retries.due(start + RETRY_DELAYS[0]), vec![Window::Agents]);
        assert_eq!(retries.next(), None, "asked again; it waits for an answer");
        assert!(
            !retries.failed(Window::Agents, start),
            "later failures are quiet"
        );
        assert_eq!(retries.next(), Some(start + RETRY_DELAYS[1]));
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

    fn test_state(root: &Path) -> st3::api::AppState {
        st3::api::AppState {
            store: Arc::new(st3::store::Store::open_memory("stui-feed").unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "stui-feed".into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: root.join("unused-pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: st3::model::PlannerSpec::default(),
        }
    }

    async fn next_window(updates: &mpsc::Receiver<Update>, wanted: Window) -> Vec<Resource> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match updates.try_recv() {
                Ok(Update::Window { window, items, .. }) if window == wanted => return items,
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "no {wanted:?} window arrived"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(mpsc::TryRecvError::Disconnected) => panic!("the feed stopped"),
            }
        }
    }

    #[tokio::test]
    async fn an_unreachable_member_falls_back_and_announces_glasses_before_their_window() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("live.sock");
        let app = st3::api::router(test_state(root.path()));
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let live = Client::unix_as(&socket, "person/avery");
        let (tx, rx) = mpsc::channel();
        let (commands, command_receiver) = channel::unbounded_channel();
        let feed = tokio::spawn(run_members(
            vec![
                Client::unix_as(root.path().join("absent.sock"), "person/avery"),
                live.clone(),
            ],
            false,
            true,
            tx,
            command_receiver,
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut connected = false;
            let mut version = None;
            loop {
                match rx.try_recv() {
                    Ok(Update::Connected(client)) => {
                        assert_eq!(format!("{client:?}"), format!("{live:?}"));
                        connected = true;
                    }
                    Ok(Update::GlassesVersion(value)) => {
                        assert!(connected);
                        version = Some(value);
                    }
                    Ok(Update::Window {
                        window: Window::Glasses,
                        ..
                    }) => {
                        assert!(version.is_some_and(|value| value >= GLASSES_VERSION));
                        break;
                    }
                    Ok(Update::Offline(reason)) => panic!("live member was available: {reason}"),
                    Ok(_) => {}
                    Err(mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(mpsc::TryRecvError::Disconnected) => panic!("the feed stopped"),
                }
            }
        })
        .await
        .unwrap();
        drop(commands);
        tokio::time::timeout(Duration::from_secs(5), feed)
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn a_cursor_from_another_member_discards_the_timeline_and_reloads_snapshots() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let app = st3::api::router(test_state(root.path()));
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !socket.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let client = Client::unix_as(&socket, "person/avery");
        let mut model = model::Model::bootstrap(&client).await.unwrap();
        model.event_cursor = "event-cursor/previous-member/0".into();
        model.timeline.push(
            serde_json::from_value(serde_json::json!({
                "id":"entry/stale", "sequence":1, "revision":1,
                "timestamp":"2026-10-01T00:00:00Z", "role":"assistant", "final":true,
                "type":"content", "body":{"media_type":"text/plain", "text":"Stale transcript"}
            }))
            .unwrap(),
        );
        let (changed, sessions, gap) =
            tokio::time::timeout(Duration::from_secs(10), model.sync(&client))
                .await
                .unwrap()
                .unwrap();
        assert!(changed && gap);
        assert!(sessions.is_empty());
        assert!(model.timeline.is_empty());
        assert_eq!(
            model.event_cursor,
            client.capabilities().await.unwrap().value.event_cursor
        );
        assert!(model.machines.snapshot.is_some());
        assert_eq!(model.status, "Resynchronized after cursor gap");
        server.abort();
    }

    #[tokio::test]
    async fn the_feed_waits_for_st_then_keeps_every_window_current_on_one_socket() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let state = test_state(root.path());
        let (tx, rx) = mpsc::channel();
        let (_commands, command_receiver) = channel::unbounded_channel();
        let feed = tokio::spawn(run(
            Client::unix_as(&socket, "person/avery"),
            tx,
            command_receiver,
        ));

        // No st yet: the feed says so and keeps trying.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(Update::Offline(_)) => break,
                Ok(other) => panic!("expected offline first, got {other:?}"),
                Err(_) => {
                    assert!(std::time::Instant::now() < deadline, "no offline update");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        let server_socket = socket.clone();
        let app = st3::api::router(state.clone());
        let server = tokio::spawn(async move { st3::api::serve_unix(&server_socket, app).await });

        for window in Window::ALL {
            assert!(next_window(&rx, window).await.is_empty());
        }

        let source =
            "version 2\nmission \"feed-test\" state=\"ready\" { goal \"Push changes to stui\" }\n";
        let intent = st3::graph::parse_intent(source, "stui-feed").unwrap();
        let planned = state
            .store
            .mission(
                &intent,
                st3::model::IntentInput {
                    kdl: source.into(),
                    source_name: None,
                },
            )
            .unwrap();
        state
            .store
            .apply(&intent, &planned.subject_tokens, "feed-test-definition")
            .unwrap();
        state
            .event_notify
            .send(state.store.index().unwrap())
            .unwrap();
        let missions = next_window(&rx, Window::Missions).await;
        assert_eq!(
            missions
                .iter()
                .map(|item| item.header().id.as_str())
                .collect::<Vec<_>>(),
            ["mission/feed-test"]
        );

        // Nothing changes, so nothing arrives: no polling.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(rx.try_recv().is_err(), "an idle feed sends nothing");
        feed.abort();
        server.abort();
    }
}
