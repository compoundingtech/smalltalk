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
    Preview(String, MissionPreview),
    Notice(String),
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
    let mut ui = Ui::new(adapt::world(&model, &person, &extras));
    ui.live = true;

    let _guard = Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let started = Instant::now();
    let mut changed = true;
    while !ui.quit {
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
                    messages.insert(agent, items);
                }
                Fetched::Preview(id, preview) => {
                    extras.previews.insert(id, preview);
                }
                Fetched::Notice(notice) => ui.flash(notice),
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
            let due = requested
                .get(&agent)
                .is_none_or(|at| at.elapsed() >= CONVERSATION_FALLBACK)
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
                if let Ok(variants) = client.launch_variants_list(&source, None, Some(50)).await {
                    let latest = variants
                        .value
                        .items
                        .iter()
                        .filter_map(|item| match item {
                            Resource::LaunchVariant(variant) => Some(variant),
                            _ => None,
                        })
                        .max_by_key(|variant| variant.ordinal);
                    if let Some(variant) = latest {
                        let _ = tx.send(Fetched::Preview(
                            id,
                            adapt::preview(&name, &variant.normalized_mission),
                        ));
                    }
                }
            });
        }

        for effect in std::mem::take(&mut ui.effects) {
            let client = client.clone();
            let tx = fetched_tx.clone();
            let person = person.clone();
            let model = model.clone();
            runtime.spawn(async move {
                let notice = match perform(&client, &person, &model, effect).await {
                    Ok(notice) => notice,
                    Err(error) => format!("Failed: {error}"),
                };
                let _ = tx.send(Fetched::Notice(notice));
            });
        }

        if changed {
            extras.conversations =
                conversations(&model, &person, &timelines, &messages, &failed, &requested);
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
    Ok(())
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

async fn perform(client: &Client, person: &str, model: &Model, effect: Effect) -> Result<String> {
    match effect {
        Effect::Attention { id, action, reason } => {
            crate::attention_action(client, person, &id, &action, reason).await
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
            Ok("Sent your changes to the planner".into())
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
            Ok("Reply sent".into())
        }
        Effect::Discuss { to, title, text } => {
            let (to, session) = (
                to.clone(),
                model
                    .agents()
                    .find(|candidate| candidate.header.id == to)
                    .and_then(|agent| agent.current_session_id.clone()),
            );
            send_titled(client, model, &to, text, Some(title), session).await?;
            Ok("Sent; the reply will show here and in their conversation".into())
        }
        Effect::Send { agent, text } => {
            let session = model
                .agents()
                .find(|candidate| candidate.header.id == agent)
                .and_then(|agent| agent.current_session_id.clone());
            send(client, model, &agent, text, None, session).await?;
            Ok("Message sent".into())
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
) -> Result<()> {
    send_message(client, model, to, content, None, in_reply_to, session_id).await
}

async fn send_titled(
    client: &Client,
    model: &Model,
    to: &str,
    content: String,
    title: Option<String>,
    session_id: Option<String>,
) -> Result<()> {
    send_message(client, model, to, content, title, None, session_id).await
}

async fn send_message(
    client: &Client,
    model: &Model,
    to: &str,
    content: String,
    title: Option<String>,
    in_reply_to: Option<String>,
    session_id: Option<String>,
) -> Result<()> {
    let snapshot = model
        .messages
        .snapshot
        .as_ref()
        .or(model.agents.snapshot.as_ref())
        .map(|snapshot| snapshot.id.clone())
        .ok_or_else(|| anyhow::anyhow!("Not connected yet"))?;
    let fence = Fence {
        snapshot_id: snapshot,
        ..Fence::default()
    };
    let (id, idem) = crate::action_pair();
    client
        .message_send(
            id,
            idem,
            fence,
            MessageSendParameters {
                to: to.to_owned(),
                content,
                title,
                in_reply_to,
                session_id,
                tags: vec![],
            },
        )
        .await?;
    Ok(())
}
