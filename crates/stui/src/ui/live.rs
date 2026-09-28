//! `stui --new`: the new screens on the live graph.
//!
//! The old stui's background sync keeps a `Model` current and sends it here. This loop turns
//! it into a `World`, fetches what only the selected item needs (a conversation, a launch
//! preview), and carries out the actions the screens queue. A refresh replaces data in
//! place: it never empties a list or a conversation while the fresh copy is on its way.

use super::adapt::{self, Extras};
use super::view::{Load, MissionPreview};
use super::{Effect, Guard, Ui};
use crate::Update;
use crate::model::{self, Model};
use anyhow::Result;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use st3_client::{
    Client, Fence, LaunchReviseParameters, MessageSendParameters, Resource, TimelineEntry,
};
use std::{
    collections::{BTreeMap, HashSet},
    io,
    sync::mpsc,
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
}

pub struct Context {
    pub client: Client,
    pub runtime: tokio::runtime::Runtime,
    pub incoming: mpsc::Receiver<Update>,
    pub person: String,
    pub cached: Option<Model>,
}

enum Fetched {
    Timeline(String, Vec<TimelineEntry>),
    Messages(String, Vec<st3_client::Message>),
    Preview(String, Load<MissionPreview>),
    /// The message behind an unread-message item: sender, title and text.
    Body(String, String, Option<String>, String),
    Notice(String),
    /// An attach finished: the agent's name and the attachment, or why it failed.
    Attached(String, Result<Box<crate::Attached>, String>),
    /// A send finished: the pending token and st's message id, or why it failed.
    Sent(String, Result<Option<String>, String>),
    Failed(String, String),
}

/// How long a conversation may go without a refetch when no event says it changed.
const CONVERSATION_FALLBACK: Duration = Duration::from_secs(45);

pub fn run(context: Context) -> Result<()> {
    let Context {
        client,
        runtime,
        incoming,
        person,
        cached,
    } = context;
    let (fetched_tx, fetched) = mpsc::channel::<Fetched>();
    let mut model = cached.unwrap_or_default();
    let mut extras = Extras::default();
    let mut timelines: BTreeMap<String, Vec<TimelineEntry>> = BTreeMap::new();
    let mut messages: BTreeMap<String, Vec<st3_client::Message>> = BTreeMap::new();
    let mut failed: BTreeMap<String, String> = BTreeMap::new();
    let mut requested: BTreeMap<String, Instant> = BTreeMap::new();
    let mut stale: HashSet<String> = HashSet::new();
    let mut preview_requested: HashSet<String> = HashSet::new();
    let mut body_requested: HashSet<String> = HashSet::new();
    // Messages sent from here, shown at once until st reports them back.
    let mut pending: Vec<Pending> = Vec::new();
    // Conversations to refresh quickly because a reply is likely soon.
    let mut hot: BTreeMap<String, Instant> = BTreeMap::new();
    let mut ui = Ui::new(adapt::world(&model, &person, &extras));
    ui.live = true;

    let _guard = Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let started = Instant::now();
    let mut changed = true;
    let stopping = super::stop_flag()?;
    let mut attached: Option<crate::Attached> = None;
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
                Update::Partial(next) => {
                    model = *next;
                    changed = true;
                }
                Update::Model(next) => {
                    model = *next;
                    extras.live = true;
                    extras.offline = None;
                    changed = true;
                    // Something changed in the graph; the open conversation may have new mail.
                    if let (1, Some(agent)) = ui.focus()
                        && requested
                            .get(&agent)
                            .is_some_and(|at| at.elapsed() > Duration::from_secs(3))
                    {
                        requested.remove(&agent);
                    }
                }
                Update::TimelineInvalidated(session) => {
                    stale.insert(session);
                }
                Update::TimelineCursorGap => {
                    stale.extend(timelines.keys().cloned());
                }
                Update::Error(error) => {
                    if error.starts_with("Sync:") || error.starts_with("Initial load:") {
                        extras.live = false;
                        extras.offline = Some(error.clone());
                        changed = true;
                    }
                    ui.flash(error);
                }
                Update::Timeline(..) | Update::Messages(..) => {}
            }
        }
        while let Ok(result) = fetched.try_recv() {
            match result {
                Fetched::Timeline(agent, entries) => {
                    failed.remove(&agent);
                    timelines.insert(agent, entries);
                }
                Fetched::Messages(agent, items) => {
                    // A pending message is done once st reports its id back.
                    pending.retain(|pending| {
                        pending
                            .message_id
                            .as_ref()
                            .is_none_or(|id| !items.iter().any(|message| &message.header.id == id))
                    });
                    messages.insert(agent, items);
                }
                Fetched::Sent(token, outcome) => {
                    if let Some(entry) = pending.iter_mut().find(|entry| entry.token == token) {
                        match outcome {
                            Ok(id) => entry.message_id = id,
                            Err(error) => entry.failed = Some(error),
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
                Fetched::Attached(name, result) => match result {
                    Ok(current) => {
                        ui.terminal = Some(super::TerminalView {
                            title: format!("{name} · {}", current.screen.title),
                            lines: screen_lines(&current.screen),
                            ended: None,
                        });
                        attached = Some(*current);
                    }
                    Err(error) => ui.flash(format!("Could not open the terminal: {error}")),
                },
                Fetched::Failed(agent, error) => {
                    failed.insert(agent, error);
                }
            }
            changed = true;
        }

        // What the selection needs: a conversation, or a launch preview.
        let (tab, selected) = ui.focus();
        if tab == 1
            && let Some(agent) = selected.clone()
        {
            let session = session_for(&model, &agent);
            let interval = if hot.get(&agent).is_some_and(|until| Instant::now() < *until) {
                Duration::from_secs(3)
            } else {
                CONVERSATION_FALLBACK
            };
            let due = requested
                .get(&agent)
                .is_none_or(|at| at.elapsed() >= interval)
                || session
                    .as_ref()
                    .is_some_and(|session| stale.contains(session));
            if due {
                requested.insert(agent.clone(), Instant::now());
                if let Some(session) = &session {
                    stale.remove(session);
                }
                fetch_conversation(&runtime, &client, &fetched_tx, agent, session);
            }
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
        // A newer screen from the attached terminal, if any.
        if let Some(current) = attached.as_mut()
            && let Some(receiver) = current.updates.as_mut()
            && receiver.has_changed().unwrap_or(false)
        {
            let update = receiver.borrow_and_update().clone();
            match update {
                Some(crate::TerminalUpdate::Screen(screen)) => {
                    current.screen = *screen;
                    if let Some(view) = ui.terminal.as_mut() {
                        view.lines = screen_lines(&current.screen);
                    }
                }
                Some(crate::TerminalUpdate::Ended(reason)) => {
                    if let Some(view) = ui.terminal.as_mut() {
                        view.ended = Some(reason);
                    }
                }
                None => {}
            }
        }
        let mut effects = Vec::new();
        for effect in std::mem::take(&mut ui.effects) {
            match effect {
                Effect::OpenTerminal { agent } => {
                    // Never block the loop on the network: resolve and attach in the background.
                    let known = model
                        .runtimes()
                        .find(|runtime| runtime.owner_id == agent && runtime.terminal_id.is_some())
                        .map(|runtime| {
                            (
                                runtime.header.id.clone(),
                                runtime.terminal_id.clone().unwrap_or_default(),
                            )
                        });
                    let ids = model
                        .agents()
                        .find(|candidate| candidate.header.id == agent)
                        .map(|candidate| candidate.runtime_ids.clone())
                        .unwrap_or_default();
                    let name = model
                        .agents()
                        .find(|candidate| candidate.header.id == agent)
                        .map(crate::agent_label)
                        .unwrap_or(agent);
                    let client = client.clone();
                    let tx = fetched_tx.clone();
                    runtime.spawn(async move {
                        let mut found = known;
                        if found.is_none() {
                            // The bounded runtime list may not include it; ask for it by id.
                            for id in ids {
                                if let Ok(envelope) = client.runtimes_get(&id).await
                                    && let Resource::Runtime(current) = envelope.value
                                    && let Some(terminal) = current.terminal_id.clone()
                                {
                                    found = Some((current.header.id.clone(), terminal));
                                    break;
                                }
                            }
                        }
                        let result = match found {
                            None => Err("that agent has no terminal right now".to_owned()),
                            Some((runtime_id, terminal_id)) => {
                                // Never wait silently: a terminal that sends no first screen is reported.
                                match tokio::time::timeout(
                                    Duration::from_secs(15),
                                    crate::attach_terminal(&client, &runtime_id, &terminal_id),
                                )
                                .await
                                {
                                    Ok(result) => {
                                        result.map(Box::new).map_err(|error| error.to_string())
                                    }
                                    Err(_) => Err(format!(
                                        "{terminal_id} sent no screen within 15 seconds"
                                    )),
                                }
                            }
                        };
                        let _ = tx.send(Fetched::Attached(name, result));
                    });
                }
                Effect::CloseTerminal => {
                    if let Some(current) = attached.take() {
                        let client = client.clone();
                        let tx = fetched_tx.clone();
                        runtime.spawn(async move {
                            if let Err(error) = crate::detach_terminal(&client, &current).await {
                                let _ = tx.send(Fetched::Notice(format!("Detach failed: {error}")));
                            }
                        });
                    }
                    ui.terminal = None;
                }
                Effect::TerminalKey(key) => {
                    if let Some(current) = &attached {
                        let client = client.clone();
                        let tx = fetched_tx.clone();
                        let terminal = current.terminal_id.clone();
                        let incarnation = current.screen.runtime_incarnation.clone();
                        runtime.spawn(async move {
                            if let Err(error) =
                                crate::send_terminal_key(&client, &terminal, &incarnation, key)
                                    .await
                            {
                                let _ = tx.send(Fetched::Notice(format!("Key not sent: {error}")));
                            }
                        });
                    }
                }
                other => effects.push(other),
            }
        }
        for effect in effects {
            let token = match &effect {
                Effect::Send { agent, text }
                | Effect::Discuss {
                    to: agent, text, ..
                } => {
                    let token = uuid::Uuid::now_v7().to_string();
                    pending.push(Pending {
                        token: token.clone(),
                        agent: agent.clone(),
                        text: text.clone(),
                        at: chrono::Local::now().format("%H:%M").to_string(),
                        message_id: None,
                        failed: None,
                    });
                    hot.insert(agent.clone(), Instant::now() + Duration::from_secs(120));
                    requested.remove(agent);
                    changed = true;
                    Some(token)
                }
                _ => None,
            };
            let client = client.clone();
            let tx = fetched_tx.clone();
            let person = person.clone();
            let model = model.clone();
            runtime.spawn(async move {
                let outcome = perform(&client, &person, &model, effect).await;
                if let Some(token) = token {
                    let _ = tx.send(Fetched::Sent(
                        token,
                        outcome
                            .as_ref()
                            .map(|(_, id)| id.clone())
                            .map_err(|error| error.to_string()),
                    ));
                }
                let _ = tx.send(Fetched::Notice(match outcome {
                    Ok((notice, _)) => notice,
                    Err(error) => format!("Failed: {error}"),
                }));
            });
        }

        if changed {
            extras.conversations =
                conversations(&model, &person, &timelines, &messages, &failed, &requested);
            for entry in &pending {
                if let Some(Load::Ready(entries)) = extras.conversations.get_mut(&entry.agent) {
                    entries.push(super::view::Entry {
                        id: format!("pending:{}", entry.token),
                        at: entry.at.clone(),
                        body: super::view::Body::Pending {
                            text: entry.text.clone(),
                            failed: entry.failed.clone(),
                        },
                    });
                }
            }
            ui.set_world(adapt::world(&model, &person, &extras));
            changed = false;
        }
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        terminal.draw(|frame| ui.render(frame))?;
        execute!(io::stdout(), EndSynchronizedUpdate)?;
        if event::poll(Duration::from_millis(80))? {
            loop {
                match event::read()? {
                    Event::Key(key) => ui.key(key),
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
        let _ = runtime.block_on(crate::detach_terminal(&client, &current));
    }
    Ok(())
}

/// A terminal screen as styled lines, the way the old screens drew it.
fn screen_lines(screen: &st3_client::TerminalScreen) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line, Span};
    screen
        .lines
        .iter()
        .map(|line| {
            if line.redacted {
                Line::from("[redacted]")
            } else if line.runs.is_empty() {
                Line::from(super::text::sanitize(&line.text))
            } else {
                Line::from(
                    line.runs
                        .iter()
                        .map(|run| {
                            Span::styled(
                                super::text::sanitize(&run.text),
                                crate::terminal_run_style(run),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            }
        })
        .collect()
}

/// The session whose transcript is this agent's conversation.
fn session_for(model: &Model, agent: &str) -> Option<String> {
    if agent.starts_with("session/") {
        return Some(agent.to_owned());
    }
    let declared = model
        .agents()
        .find(|candidate| candidate.header.id == agent)?;
    declared.current_session_id.clone().or_else(|| {
        model.sessions.items.iter().find_map(|item| match item {
            Resource::Session(session)
                if session.owner_id == agent && session.state == "running" =>
            {
                Some(session.header.id.clone())
            }
            _ => None,
        })
    })
}

fn fetch_conversation(
    runtime: &tokio::runtime::Runtime,
    client: &Client,
    tx: &mpsc::Sender<Fetched>,
    agent: String,
    session: Option<String>,
) {
    if let Some(session) = session {
        let client = client.clone();
        let tx = tx.clone();
        let agent = agent.clone();
        runtime.spawn(async move {
            let mut model = Model::default();
            match model
                .load_timeline_with_pages(&client, &session, model::MAX_PAGES)
                .await
            {
                Ok(()) => {
                    let _ = tx.send(Fetched::Timeline(agent, model.timeline));
                }
                Err(error) => {
                    let _ = tx.send(Fetched::Failed(agent, error.to_string()));
                }
            }
        });
    }
    if agent.starts_with("agent/") {
        let client = client.clone();
        let tx = tx.clone();
        runtime.spawn(async move {
            let mut model = Model::default();
            if model.load_messages_for_peer(&client, &agent).await.is_ok() {
                let items = model
                    .messages
                    .items
                    .into_iter()
                    .filter_map(|item| match item {
                        Resource::Message(message) => Some(message),
                        _ => None,
                    })
                    .collect();
                let _ = tx.send(Fetched::Messages(agent, items));
            }
        });
    }
}

fn conversations(
    model: &Model,
    person: &str,
    timelines: &BTreeMap<String, Vec<TimelineEntry>>,
    messages: &BTreeMap<String, Vec<st3_client::Message>>,
    failed: &BTreeMap<String, String>,
    requested: &BTreeMap<String, Instant>,
) -> BTreeMap<String, Load<Vec<super::view::Entry>>> {
    let mut out = BTreeMap::new();
    let names = adapt::names(model, person);
    for agent in requested.keys() {
        let timeline = timelines.get(agent);
        let mail = messages.get(agent);
        let load = match (timeline, mail) {
            (None, None) => match failed.get(agent) {
                Some(error) => Load::Failed(format!("Could not load this conversation: {error}")),
                None if agent.starts_with("session/") || session_for(model, agent).is_some() => {
                    Load::Loading
                }
                None => Load::Failed(
                    "This agent has no running session, so there is no transcript to show.".into(),
                ),
            },
            _ => Load::Ready(adapt::conversation(
                timeline.map(Vec::as_slice).unwrap_or(&[]),
                mail.map(Vec::as_slice).unwrap_or(&[]),
                &names,
            )),
        };
        out.insert(agent.clone(), load);
    }
    out
}

async fn perform(
    client: &Client,
    person: &str,
    model: &Model,
    effect: Effect,
) -> Result<(String, Option<String>)> {
    match effect {
        Effect::Attention { id, action, reason } => {
            crate::attention_action(client, person, &id, &action, reason)
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
            send(
                client,
                model,
                &to,
                text,
                Some(attention.source_id.clone()),
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
            let id = send_titled(client, model, &to, text, Some(title), session).await?;
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
        Effect::Send { agent, text } => {
            let session = model
                .agents()
                .find(|candidate| candidate.header.id == agent)
                .and_then(|agent| agent.current_session_id.clone());
            let id = send(client, model, &agent, text, None, session).await?;
            Ok(("Message sent".into(), id))
        }
    }
}

async fn send(
    client: &Client,
    model: &Model,
    to: &str,
    content: String,
    in_reply_to: Option<String>,
    session_id: Option<String>,
) -> Result<Option<String>> {
    send_message(client, model, to, content, None, in_reply_to, session_id).await
}

async fn send_titled(
    client: &Client,
    model: &Model,
    to: &str,
    content: String,
    title: Option<String>,
    session_id: Option<String>,
) -> Result<Option<String>> {
    send_message(client, model, to, content, title, None, session_id).await
}

async fn send_message(
    client: &Client,
    _model: &Model,
    to: &str,
    content: String,
    title: Option<String>,
    in_reply_to: Option<String>,
    session_id: Option<String>,
) -> Result<Option<String>> {
    // A send only needs a current snapshot. Take a fresh one each time, and once more if the
    // graph moves between reading it and sending: a stale fence is not the person's problem.
    let mut last = None;
    for _ in 0..2 {
        let snapshot = client.capabilities().await?.snapshot.id;
        let fence = Fence {
            snapshot_id: snapshot,
            ..Fence::default()
        };
        let (id, idem) = crate::action_pair();
        match client
            .message_send(
                id,
                idem,
                fence,
                MessageSendParameters {
                    to: to.to_owned(),
                    content: content.clone(),
                    title: title.clone(),
                    in_reply_to: in_reply_to.clone(),
                    session_id: session_id.clone(),
                    tags: vec![],
                },
            )
            .await
        {
            Ok(result) => {
                // The new message's id, so the pending copy can give way to the real one.
                return Ok(result
                    .value
                    .affected_ids
                    .into_iter()
                    .find(|id| id.starts_with("message/")));
            }
            Err(error) if error.to_string().contains("StaleFence") => last = Some(error),
            Err(error) => return Err(error.into()),
        }
    }
    Err(last
        .map(Into::into)
        .unwrap_or_else(|| anyhow::anyhow!("the graph kept changing; try again")))
}
