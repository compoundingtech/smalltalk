//! `stui`: the screens on the live graph.
//!
//! The feed keeps the attention, missions and agents windows current over one socket, joined
//! by st, and follows the open terminal on the same socket. This loop turns those windows
//! into a `World`, fetches what only the selected item or tab needs (a conversation, a launch
//! preview, the fleet), and carries out the actions the screens queue. A refresh replaces
//! data in place: it never empties a list or a conversation while the fresh copy is on its way.

use super::adapt::{self, Extras};
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
    Client, Fence, LaunchReviseParameters, MessageSendParameters, Resource, TimelineBody,
    TimelineEntry,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
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
    pub incoming: mpsc::Receiver<feed::Update>,
    pub commands: tokio::sync::mpsc::UnboundedSender<Command>,
    pub person: String,
    pub cache_path: Option<std::path::PathBuf>,
    pub cached: Option<Model>,
}

/// The terminal the feed follows for the open terminal view.
struct Following {
    terminal_id: String,
    attachment_id: String,
    incarnation: String,
}

enum Fetched {
    Preview(String, Load<MissionPreview>),
    /// The message behind an unread-message item: sender, title and text.
    Body(String, String, Option<String>, String),
    Notice(String),
    /// Harness sessions st did not start, found on this machine.
    Sessions(Collection),
    /// The Fleet tab's machines and paired devices.
    Machines(Collection),
    Devices(Collection),
    /// A send finished: the pending token and st's message id, or why it failed.
    Sent(String, Result<Option<String>, String>),
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
    } = context;
    let (fetched_tx, fetched) = mpsc::channel::<Fetched>();
    let mut model = cached.unwrap_or_default();
    // The attention window is already this person's; nothing else names the actor.
    model.actor = person.clone();
    let mut extras = Extras::default();
    // Each conversation st has sent, kept after it closes so reopening it shows its last entries.
    let mut timelines: BTreeMap<String, Vec<TimelineEntry>> = BTreeMap::new();
    let mut failed: BTreeMap<String, String> = BTreeMap::new();
    // The agent or session whose conversation the feed holds.
    let mut conversing: Option<String> = None;
    let mut preview_requested: HashSet<String> = HashSet::new();
    let mut body_requested: HashSet<String> = HashSet::new();
    // Messages sent from here, shown at once until st reports them back.
    let mut pending: Vec<Pending> = Vec::new();
    let mut ui = Ui::new(adapt::world(&model, &person, &extras));
    ui.live = true;

    let _guard = Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.hide_cursor()?;
    let started = Instant::now();
    let mut changed = true;
    let stopping = super::stop_flag()?;
    let mut attached: Option<Following> = None;
    // The runtimes of the agent whose terminal view is open, to follow it again after a pause.
    let mut terminal_runtimes: Option<Vec<String>> = None;
    // The tab shown on the last pass: opening a tab loads what only it needs.
    let mut shown_tab = usize::MAX;
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
                    window,
                    snapshot,
                    items,
                    has_more,
                } => {
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
                    }
                    extras.live = true;
                    extras.offline = None;
                    model.last_connected = Some(
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    );
                    changed = true;
                }
                feed::Update::Conversation {
                    target,
                    replace,
                    items,
                } => {
                    failed.remove(&target);
                    let entries = timelines.entry(target).or_default();
                    if replace {
                        *entries = items;
                    } else {
                        for item in items {
                            match entries.iter_mut().find(|entry| entry.id == item.id) {
                                Some(entry) => *entry = item,
                                None => entries.push(item),
                            }
                        }
                        entries.sort_by(|a, b| {
                            a.timestamp
                                .cmp(&b.timestamp)
                                .then(a.sequence.cmp(&b.sequence))
                        });
                    }
                    // A message sent from here is done once st shows it in the conversation.
                    pending.retain(|pending| {
                        pending.message_id.as_ref().is_none_or(|id| {
                            !timelines.values().flatten().any(|entry| {
                                matches!(&entry.body, TimelineBody::Message(message) if &message.message_id == id)
                            })
                        })
                    });
                    changed = true;
                }
                feed::Update::ConversationFailed { target, message } => {
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
                Fetched::Sessions(native) => {
                    model.sessions = native;
                }
                Fetched::Machines(machines) => model.machines = machines,
                Fetched::Devices(devices) => model.devices = devices,
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
            if tab == 3 {
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
        // The selected agent's conversation rides the feed's socket while the Agents tab is
        // open: st pushes each change, so nothing here reads it again on a timer.
        let wanted = selected.clone().filter(|_| tab == 1);
        if wanted != conversing {
            let _ = commands.send(match &wanted {
                Some(target) => Command::Converse {
                    target: target.clone(),
                },
                None => Command::Unconverse,
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
            if !extras.live && !matches!(effect, Effect::CloseTerminal) {
                ui.flash("Offline · reconnect before acting; nothing was queued");
                continue;
            }
            match effect {
                Effect::OpenTerminal { agent } => {
                    // The feed attaches and follows on its socket; screens arrive as updates.
                    let found = model
                        .agents()
                        .find(|candidate| candidate.header.id == agent);
                    let runtime_ids = found
                        .map(|candidate| candidate.runtime_ids.clone())
                        .unwrap_or_default();
                    let name = found
                        .map(crate::agent_label)
                        .unwrap_or_else(|| agent.clone());
                    attached = None;
                    terminal_runtimes = Some(runtime_ids.clone());
                    if commands.send(Command::Follow { runtime_ids }).is_ok() {
                        ui.terminal = Some(super::TerminalView {
                            agent: agent.clone(),
                            title: name.clone(),
                            name,
                            lines: Vec::new(),
                            cursor: None,
                            stale: Some("connecting".into()),
                            ended: None,
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
                                let _ = tx.send(Fetched::Notice(format!("Key not sent: {error}")));
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
                conversations(&model, &person, &timelines, &failed, conversing.as_deref());
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
            if last_cache_save.elapsed() >= Duration::from_secs(60) {
                save_cache(cache_path.as_deref(), &person, &model);
                last_cache_save = Instant::now();
            }
        }
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        terminal.draw(|frame| ui.render(frame))?;
        execute!(io::stdout(), EndSynchronizedUpdate)?;
        if event::poll(Duration::from_millis(80))? {
            loop {
                match event::read()? {
                    Event::Key(key)
                        if !extras.live
                            && key.code == crossterm::event::KeyCode::Char('r')
                            && !ui.editing =>
                    {
                        let _ = commands.send(Command::Reconnect);
                    }
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
    timelines: &BTreeMap<String, Vec<TimelineEntry>>,
    failed: &BTreeMap<String, String>,
    conversing: Option<&str>,
) -> BTreeMap<String, Load<Vec<super::view::Entry>>> {
    let mut out = BTreeMap::new();
    let names = adapt::names(model, person);
    let targets = timelines
        .keys()
        .chain(failed.keys())
        .map(String::as_str)
        .chain(conversing)
        .collect::<BTreeSet<_>>();
    for target in targets {
        let load = match (timelines.get(target), failed.get(target)) {
            (Some(timeline), error) => {
                let mut entries = adapt::conversation(timeline, &names);
                // Never hide a failure behind what loaded before it.
                if let Some(error) = error {
                    entries.push(super::view::Entry {
                        id: format!("failed:{target}"),
                        at: String::new(),
                        body: super::view::Body::Event(format!(
                            "Could not load newer entries: {error}"
                        )),
                    });
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

#[cfg(test)]
mod tests {
    use super::*;

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
