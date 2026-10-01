//! One socket to st for everything stui shows live.
//!
//! The feed holds one collection socket with the attention, missions and agents windows, the
//! open conversation, and, while a terminal is open, that terminal. st joins each row and each
//! conversation, so stui never joins lists itself or reads item by item. When the socket drops,
//! the feed opens a new one, subscribes again, and attaches the open terminal again. Nothing
//! here polls projections while connected: frames arrive only when something changed. Remote
//! devices also make a bounded liveness read so an idle network blackhole becomes offline.

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
    /// The person's glasses, followed only by `stui --glasses` when st grants them.
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
        replace: bool,
        has_more: bool,
        items: Vec<TimelineEntry>,
    },
    /// st could not show the open conversation; the feed asks again after a backoff.
    ConversationFailed {
        target: String,
        message: String,
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

/// Keep stui's windows and terminal current until the receiving side goes away.
#[cfg(test)]
pub async fn run(
    client: Client,
    updates: mpsc::Sender<Update>,
    commands: channel::UnboundedReceiver<Command>,
) {
    run_members(vec![client], false, false, updates, commands).await;
}

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
    /// stui is closing.
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
    // A blackholed network can leave a WebSocket open indefinitely. A bounded read
    // detects that case even when the graph is idle; it never resends a mutation.
    let mut probe = tokio::time::interval(Duration::from_secs(15));
    probe.tick().await;
    // Glasses are followed only where st grants them in the shape this stui reads (splits of
    // tab groups, version 1); elsewhere stui keeps them on the device. A member still on the
    // earlier shape would send glasses this stui cannot decode, and that drops the connection.
    let granted = glasses
        && client.capabilities().await.is_ok_and(|capabilities| {
            capabilities.value.capabilities.iter().any(|capability| {
                capability.id == "glasses"
                    && capability.version >= GLASSES_VERSION
                    && capability.state == CapabilityState::Granted
            })
        });
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
        tokio::select! {
            _ = probe.tick(), if remote => {
                match tokio::time::timeout(Duration::from_secs(5), client.capabilities()).await {
                    Ok(Ok(_)) => *failures = 0,
                    Ok(Err(error)) => return Ended::Dropped(error.to_string()),
                    Err(_) => return Ended::Dropped("Member stopped answering".into()),
                }
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
                    CollectionEvent::Resync { id } => {
                        if let Some(window) = Window::from_id(&id)
                            && let Err(error) = stream.subscribe(window.id(), window.id(), window.limit(), None, None).await
                        {
                            return Ended::Dropped(error.to_string());
                        }
                    }
                    CollectionEvent::Conversation { id, replace, items, has_more, .. } => {
                        let Some(current) = conversing.iter_mut().find(|current| current.id == id) else { continue };
                        current.failures = 0;
                        if updates.send(Update::Conversation { target: current.target.clone(), replace, has_more, items }).is_err() {
                            return Ended::Closed;
                        }
                    }
                    CollectionEvent::Error { id, message, .. } if id.starts_with(CONVERSATION) => {
                        let Some(current) = conversing.iter_mut().find(|current| current.id == id) else { continue };
                        // Anything may clear: the agent starts, its host comes back, its
                        // history arrives. Ask again after a backoff.
                        current.retry_at = Some(Instant::now() + RETRY_DELAYS[current.failures.min(RETRY_DELAYS.len() - 1)]);
                        current.failures += 1;
                        if updates.send(Update::ConversationFailed { target: current.target.clone(), message }).is_err() {
                            return Ended::Closed;
                        }
                    }
                    CollectionEvent::Error { id, code, message } => {
                        if let Some(window) = Window::from_id(&id) {
                            if updates.send(Update::WindowFailed(window, message)).is_err() {
                                return Ended::Closed;
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

/// A terminal subscription ended with an error. Transient failures attach again after a
/// backoff; `stale-fence` means the terminal restarted, and anything else stops following.
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
    let update = match code {
        Some(ErrorCode::StaleFence) => {
            *following = None;
            TerminalUpdate::Ended {
                restarted: true,
                reason: "Terminal restarted".into(),
            }
        }
        Some(ErrorCode::Internal | ErrorCode::RemoteUnavailable | ErrorCode::RateLimited) => {
            current.retry_at =
                Some(Instant::now() + RETRY_DELAYS[current.failures.min(RETRY_DELAYS.len() - 1)]);
            current.failures += 1;
            TerminalUpdate::Reconnecting(message)
        }
        _ => {
            *following = None;
            TerminalUpdate::Ended {
                restarted: false,
                reason: message,
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
    Failed(String),
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

/// `terminal.attach` with a fresh fence, retried on `stale-fence` three times. With `expected`,
/// a runtime now on another incarnation is a restart rather than something to attach to.
async fn attach(
    client: &Client,
    runtime_id: &str,
    terminal_id: &str,
    expected: Option<&str>,
) -> Result<st3_client::TerminalAttachment, Refusal> {
    for attempt in 0..3 {
        let current = client
            .runtimes_get(runtime_id)
            .await
            .map_err(|error| Refusal::Failed(error.to_string()))?;
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
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt < 2 => continue,
            Err(error) => return Err(Refusal::Failed(error.to_string())),
        }
    }
    Err(Refusal::Failed(
        "the terminal kept changing while attaching".into(),
    ))
}

/// End a viewer record with a fresh fence, retried on `stale-fence` three times.
pub async fn detach(
    client: &Client,
    terminal_id: &str,
    attachment_id: &str,
    incarnation: &str,
) -> anyhow::Result<()> {
    for attempt in 0..3 {
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
            Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt < 2 => continue,
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
    fn stale_fence_ends_following_as_a_restart_and_never_reattaches() {
        let (tx, rx) = mpsc::channel();
        let mut current = following();
        terminal_failed(
            &tx,
            &mut current,
            Some(ErrorCode::StaleFence),
            "stale".into(),
        );
        assert!(
            current.is_none(),
            "a restart is never followed again on its own"
        );
        assert!(matches!(
            terminal_update(&rx),
            TerminalUpdate::Ended {
                restarted: true,
                ..
            }
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
            TerminalUpdate::Ended { restarted: false, reason } if reason == "no"
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
