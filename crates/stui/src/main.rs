mod connection;
mod ui;
mod version;
mod voice;

use st3_feed as feed;
#[cfg(test)]
use st3_feed::terminal_screen_fence;
use st3_feed::{action_pair, cache, model, terminal_fence};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use st3_client::{
    Client, ClientError, ErrorCode, Fence, LaunchVariantParameters, PersonStepParameters, Resource,
    TargetParameters, TerminalInputMode, TerminalInputParameters,
};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool, mpsc},
    time::Duration,
};

fn mission_label(mission: &st3_client::Mission) -> String {
    let slug = mission.title.rsplit('/').next().unwrap_or(&mission.title);
    slug.split('-')
        .map(|word| match word.to_ascii_lowercase().as_str() {
            "tui" => "TUI".into(),
            "ios" => "iOS".into(),
            "st3" => "ST".into(),
            "omp" => "OMP".into(),
            "api" => "API".into(),
            "pty" => "PTY".into(),
            _ => {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn mission_display_label(mission: &st3_client::Mission) -> String {
    let path = mission
        .header
        .id
        .strip_prefix("mission/")
        .unwrap_or(&mission.header.id);
    let scope = path.rsplit_once('/').map_or(path, |(scope, _)| scope);
    format!("{} · {}", scope, mission_label(mission))
}
fn action_label(action: &str) -> String {
    match action {
        "custom.reply" => "Reply with the declared fields".into(),
        "work.done" => "Complete step [c]".into(),
        "review.approve" | "launch.approve" => "Approve [a]".into(),
        "review.reject" => "Reject [j]".into(),
        "review.request-changes" => "Request changes [r]".into(),
        "launch.cancel" => "Cancel [d]".into(),
        "mission.approve-revision" => "Approve revision [CLI]".into(),
        "mission.cancel-revision" => "Cancel revision [CLI]".into(),
        "message.read" => "Mark read [m]".into(),
        other => format!("{} [CLI]", other.replace('.', " ")),
    }
}
fn age_label(then: &str, now: &str) -> String {
    let parsed = chrono::DateTime::parse_from_rfc3339(then).ok();
    let current = chrono::DateTime::parse_from_rfc3339(now).ok();
    let Some(seconds) = parsed
        .zip(current)
        .map(|(then, now)| (now - then).num_seconds().max(0))
    else {
        return "unknown age".into();
    };
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}
fn person_from_config(path: &std::path::Path) -> Result<Option<String>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: toml::Value = toml::from_str(&raw)?;
    Ok(value
        .get("person")
        .and_then(toml::Value::as_str)
        .map(str::to_owned))
}
fn configured_person() -> Result<Option<String>> {
    if let Ok(person) = std::env::var("ST3_PERSON") {
        return Ok(Some(person));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    base.map(|base| person_from_config(&base.join("st3/config.toml")))
        .transpose()
        .map(Option::flatten)
}
fn stdin_hung_up() -> bool {
    if !io::stdin().is_terminal() {
        return true;
    }
    let mut fd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    // A closed PTY master leaves a POLLHUP/POLLERR on the slave; crossterm's
    // event reader can otherwise spin or block after the terminal disappears.
    let result = unsafe { libc::poll(&mut fd, 1, 0) };
    result > 0 && terminal_closed(fd.revents)
}
fn terminal_closed(revents: libc::c_short) -> bool {
    if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    #[cfg(target_os = "macos")]
    if revents & libc::POLLIN != 0 {
        // Darwin reports a tmux pane's closed slave as readable EOF without POLLHUP.
        // crossterm then reads zero bytes in a tight loop unless we check the queue.
        let mut queued: libc::c_int = 0;
        if unsafe { libc::ioctl(0, libc::FIONREAD, &mut queued) } == 0 && queued == 0 {
            return true;
        }
    }
    false
}
fn watch_terminal_hangup() {
    // crossterm can loop inside event::poll (Darwin) or event::read (Linux) on PTY EOF and
    // never return to the main loop's terminal check; a stui left like that spins at full CPU
    // and keeps polling the daemon for hours. A separate poller observes the hangup without
    // consuming input. Once the PTY is gone there is no terminal left to restore, so end the
    // process even if crossterm is stuck.
    std::thread::spawn(|| {
        loop {
            let mut fd = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut fd, 1, 100) } > 0 && terminal_closed(fd.revents) {
                std::thread::sleep(Duration::from_millis(25));
                if stdin_hung_up() {
                    std::process::exit(0);
                }
            }
        }
    });
}
fn agent_label(agent: &st3_client::Agent) -> String {
    let slug = agent.name.rsplit('/').next().unwrap_or(&agent.name);
    // `st agents new NAME` names a seat HOST.NAME; the host shows beside it, so the label is
    // NAME ("harbor.image-sorter" reads as Image Sorter).
    let host = agent
        .host_id
        .as_deref()
        .map(|host| host.trim_start_matches("host/"));
    let slug = host
        .and_then(|host| {
            slug.split_once('.')
                .filter(|(prefix, rest)| prefix.eq_ignore_ascii_case(host) && !rest.is_empty())
        })
        .map_or(slug, |(_, rest)| rest);
    let label = |slug: &str| {
        slug.split('-')
            .map(|word| match word.to_ascii_lowercase().as_str() {
                "st3" => "ST".to_string(),
                "cos" => "COS".to_string(),
                "ios" => "iOS".to_string(),
                "tui" => "TUI".to_string(),
                "pty" => "PTY".to_string(),
                "omp" => "OMP".to_string(),
                _ => {
                    let mut chars = word.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                        .unwrap_or_default()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    if slug.eq_ignore_ascii_case("omp") {
        if let Some(parent) = agent.name.rsplit('/').nth(1) {
            return format!("{} · OMP", label(parent));
        }
    }
    label(slug)
}
fn key_input(key: KeyEvent) -> Option<String> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => Some(format!("C-{c}")),
        KeyCode::Char(c) => Some(c.to_string()),
        KeyCode::Enter => Some("return".into()),
        KeyCode::Tab => Some("tab".into()),
        KeyCode::Backspace => Some("backspace".into()),
        KeyCode::Esc => Some("escape".into()),
        KeyCode::Up => Some("Up".into()),
        KeyCode::Down => Some("Down".into()),
        KeyCode::Left => Some("Left".into()),
        KeyCode::Right => Some("Right".into()),
        KeyCode::Delete => Some("Delete".into()),
        KeyCode::Home => Some("Home".into()),
        KeyCode::End => Some("End".into()),
        _ => None,
    }
}

fn terminal_run_style(run: &st3_client::TerminalRun) -> Style {
    let color = |color: &st3_client::TerminalColor| match color {
        st3_client::TerminalColor::Palette(index) => Some(Color::Indexed(*index)),
        st3_client::TerminalColor::Rgb(hex) => {
            let value = u32::from_str_radix(hex.strip_prefix('#')?, 16).ok()?;
            Some(Color::Rgb(
                (value >> 16) as u8,
                (value >> 8) as u8,
                value as u8,
            ))
        }
    };
    let mut style = Style::default();
    if let Some(fg) = run.fg.as_ref().and_then(color) {
        style = style.fg(fg);
    }
    if let Some(bg) = run.bg.as_ref().and_then(color) {
        style = style.bg(bg);
    }
    for (set, modifier) in [
        (run.bold, Modifier::BOLD),
        (run.dim, Modifier::DIM),
        (run.italic, Modifier::ITALIC),
        (run.underline, Modifier::UNDERLINED),
        (run.inverse, Modifier::REVERSED),
    ] {
        if set {
            style = style.add_modifier(modifier);
        }
    }
    style
}

/// Perform one attention action against fresh fences. Shared by the old and new screens.
/// `seen` is the card as the person saw it, which names its source if st has since closed it.
async fn attention_action(
    client: &Client,
    actor: &str,
    attention_id: &str,
    seen: Option<&st3_client::Attention>,
    action: &str,
    reason: Option<String>,
    answer: Option<String>,
) -> Result<String> {
    let current = client.attention_get(attention_id).await;
    act_on_attention(client, actor, current, seen, action, reason, answer).await
}

/// Act on `current`, a card as st showed it. A review whose card changed (a stale fence) or
/// closed (not found) before the action arrived was asked again or answered: the action goes
/// once more to the card st shows now for the same source, if it still asks, and otherwise
/// says why not. Only once, so a gate st keeps asking again is reported, not chased.
async fn act_on_attention(
    client: &Client,
    actor: &str,
    current: std::result::Result<st3_client::Envelope<Resource>, ClientError>,
    seen: Option<&st3_client::Attention>,
    action: &str,
    reason: Option<String>,
    answer: Option<String>,
) -> Result<String> {
    let mut source = seen.map(|card| (card.source_id.clone(), card.attention_kind.clone()));
    let first = match current {
        Ok(current) => {
            if let Resource::Attention(card) = &current.value {
                source = Some((card.source_id.clone(), card.attention_kind.clone()));
            }
            act_on_card(
                client,
                actor,
                current,
                action,
                reason.clone(),
                answer.clone(),
            )
            .await
        }
        Err(error) => Err(error.into()),
    };
    match (first, &source) {
        (Err(error), Some((_, kind))) if kind == "human-gate" && card_moved_on(&error) => {}
        (outcome, _) => return outcome,
    }
    let (source, kind) = source.expect("a review names its source");
    match current_card(client, actor, &source, &kind).await? {
        // The person answers what they read: a card that now says something else, such as a
        // new attempt's work, is shown again instead of answered on their behalf.
        Some(card)
            if seen.is_some_and(|seen| {
                matches!(&card.value, Resource::Attention(now) if (&now.title, &now.detail, &now.what, &now.because)
                    != (&seen.title, &seen.detail, &seen.what, &seen.because))
            }) =>
        {
            Err(anyhow::anyhow!(
                "This review changed since you read it; read it again before you answer"
            ))
        }
        Some(card) => act_on_card(client, actor, card, action, reason, answer).await,
        // The person reads why, not the stale fence that found it out.
        None => Err(anyhow::anyhow!(no_longer_asked(client, &source).await)),
    }
}

/// Whether an action failed because its card is no longer the one st shows: st asked its
/// source again (a stale fence) or closed it (not found).
fn card_moved_on(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<ClientError>())
        .any(|error| {
            matches!(
                error,
                ClientError::Api(ErrorCode::StaleFence | ErrorCode::NotFound, ..)
            )
        })
}

/// The card st shows `actor` now for `source`, of `kind`, with the snapshot it was read at.
async fn current_card(
    client: &Client,
    actor: &str,
    source: &str,
    kind: &str,
) -> Result<Option<st3_client::Envelope<Resource>>> {
    let mut cursor = None;
    loop {
        let page = client
            .attention_list(cursor.as_deref(), Some(100), false)
            .await?;
        let card = page.value.items.iter().find(|item| {
            matches!(item, Resource::Attention(card)
                if card.source_id == source && card.attention_kind == kind
                    && card.person_id == actor && card.state != "resolved")
        });
        if let Some(card) = card {
            return Ok(Some(st3_client::Envelope {
                api_version: page.api_version,
                request_id: page.request_id,
                snapshot: page.snapshot,
                value: card.clone(),
            }));
        }
        match page.value.page.next_cursor {
            Some(next) if page.value.page.has_more => cursor = Some(next),
            _ => return Ok(None),
        }
    }
}

/// Why st no longer asks the person about `source`, from its step's state when it has one.
async fn no_longer_asked(client: &Client, source: &str) -> String {
    let step = match client.work_get(source).await {
        Ok(work) => match work.value {
            Resource::Work(work) => Some(work.state),
            _ => None,
        },
        Err(_) => None,
    };
    match step.as_deref() {
        Some("completed") => {
            "This review is no longer asked: the step is completed, so it was already answered"
                .into()
        }
        Some(state @ ("failed" | "cancelled")) => {
            format!("This review is no longer asked: the step is {state}")
        }
        Some(state) => format!(
            "This review is no longer asked: the step is {state}; a new card appears if st asks again"
        ),
        None => {
            "This review is no longer asked: it was answered, or what it reviewed moved on".into()
        }
    }
}

/// The typed answer for a person step: the named answer chosen, or the person's own words where
/// the request takes them. Words alone on a structured request were refused (`answer-required`),
/// so an answer typed into a custom choice left the item on Home (Nathan, 2026-10-03).
fn person_answer(
    request: Option<&st3_client::StructuredRequest>,
    chosen: Option<String>,
    words: &str,
) -> Result<Option<st3_client::PersonAnswerInput>> {
    if let Some(id) = chosen {
        // Requesting changes carries the changes, in the person's words.
        let changes = request.is_some_and(|request| {
            request.answers.iter().any(|answer| {
                answer.id == id && answer.outcome.as_deref() == Some("request_changes")
            })
        });
        return Ok(Some(st3_client::PersonAnswerInput {
            id: Some(id),
            text: changes.then(|| words.trim().to_owned()),
        }));
    }
    let Some(request) = request else {
        return Ok(None);
    };
    let text = Some(words.trim().to_owned());
    match request.entry_type.as_str() {
        "feedback" => Ok(Some(st3_client::PersonAnswerInput { id: None, text })),
        "choice" if request.custom => Ok(Some(st3_client::PersonAnswerInput { id: None, text })),
        // A decision takes words as its request for changes, when it offers one.
        _ => match request
            .answers
            .iter()
            .find(|answer| answer.outcome.as_deref() == Some("request_changes"))
        {
            Some(changes) => Ok(Some(st3_client::PersonAnswerInput {
                id: Some(changes.id.clone()),
                text,
            })),
            None => anyhow::bail!("This asks you to pick one of its answers: a chooses one"),
        },
    }
}

/// Perform `action` on `current`, a card as st showed it, fenced on that card.
async fn act_on_card(
    client: &Client,
    actor: &str,
    current: st3_client::Envelope<Resource>,
    action: &str,
    reason: Option<String>,
    answer: Option<String>,
) -> Result<String> {
    let Resource::Attention(attention) = &current.value else {
        anyhow::bail!("Attention changed; refresh and choose again");
    };
    anyhow::ensure!(
        attention.person_id == actor
            && attention
                .actions
                .iter()
                .any(|available| available == action),
        "Action is no longer available"
    );
    let mut fence = Fence {
        snapshot_id: current.snapshot.id,
        ..Fence::default()
    };
    fence.subject_revisions.insert(
        attention.header.id.clone(),
        attention.header.revision.clone(),
    );
    let source = if action.starts_with("launch.") {
        attention.launch_id.clone().unwrap_or_else(|| {
            attention
                .source_id
                .strip_prefix("planning-session/")
                .unwrap_or(&attention.source_id)
                .to_owned()
        })
    } else {
        attention.source_id.clone()
    };
    if action.starts_with("launch.") {
        let launch = client.launches_get(&source).await?;
        let Resource::Launch(launch_resource) = launch.value else {
            anyhow::bail!("Launch is no longer available");
        };
        fence.snapshot_id = launch.snapshot.id;
        fence
            .subject_revisions
            .insert(launch_resource.header.id, launch_resource.header.revision);
    }
    let (id, idem) = action_pair();
    let result = match action {
        "custom.reply" => {
            let text = reason.ok_or_else(|| anyhow::anyhow!("enter a reply"))?;
            let fields = custom_form_input(attention.custom_form.as_ref(), &text)?;
            let mut parameters: st3_client::CustomReplyParameters = serde_json::from_value({
                let mut p = attention.action_parameters["custom.reply"].clone();
                p["fields"] = serde_json::json!({});
                p
            })?;
            parameters.fields = fields;
            client.custom_reply(id, idem, fence, parameters).await?
        }
        "work.done" => {
            let summary = reason
                .filter(|summary| !summary.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("a person step needs a response"))?;
            let answer = person_answer(attention.request.as_ref(), answer, &summary)?;
            client
                .work_done(
                    id,
                    idem,
                    fence,
                    PersonStepParameters {
                        target_id: source,
                        episode: attention.episode.clone(),
                        summary,
                        evidence: vec![],
                        answer,
                    },
                )
                .await?
        }
        "review.approve" => {
            client
                .review_approve(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        reason,
                        ..Default::default()
                    },
                )
                .await?
        }
        "review.reject" => {
            client
                .review_reject(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        reason,
                        ..Default::default()
                    },
                )
                .await?
        }
        "review.request-changes" => {
            client
                .review_request_changes(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        reason,
                        ..Default::default()
                    },
                )
                .await?
        }
        "message.read" => {
            client
                .message_read(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        ..Default::default()
                    },
                )
                .await?
        }
        "launch.cancel" => {
            client
                .launch_cancel(
                    id,
                    idem,
                    fence,
                    TargetParameters {
                        target_id: source,
                        ..Default::default()
                    },
                )
                .await?
        }
        "launch.approve" => {
            let variants = client.launch_variants_list(&source, None, Some(50)).await?;
            let variant = variants
                .value
                .items
                .iter()
                .filter_map(|item| match item {
                    Resource::LaunchVariant(variant)
                        if variant.preview_token.is_some()
                            && attention
                                .variant_id
                                .as_deref()
                                .is_none_or(|id| id == variant.header.id) =>
                    {
                        Some(variant)
                    }
                    _ => None,
                })
                .max_by_key(|variant| variant.ordinal)
                .context("No current launch preview; review it in the CLI")?;
            fence.preview_token = variant.preview_token.clone();
            client
                .launch_approve(
                    id,
                    idem,
                    fence,
                    LaunchVariantParameters {
                        launch_id: source,
                        variant_id: variant.header.id.clone(),
                    },
                )
                .await?
        }
        _ => anyhow::bail!("This action needs the CLI: {action}"),
    };
    Ok(format!("{}: {}", action_label(action), result.value.kind))
}
async fn send_terminal_key(
    client: &Client,
    terminal_id: &str,
    incarnation: &str,
    key: KeyEvent,
) -> Result<()> {
    if let Some(value) = key_input(key) {
        // A key races a busy store's fence; a fresh read and a short pause usually land it.
        for attempt in 0..8 {
            let fence = terminal_fence(client, terminal_id, incarnation).await?;
            let (id, idem) = action_pair();
            match client
                .terminal_input(
                    id,
                    idem,
                    fence,
                    TerminalInputParameters {
                        terminal_id: terminal_id.to_owned(),
                        mode: TerminalInputMode::Key,
                        value: value.clone(),
                    },
                )
                .await
            {
                Ok(_) => break,
                Err(ClientError::Api(ErrorCode::StaleFence, _, _)) if attempt < 7 => {
                    tokio::time::sleep(std::time::Duration::from_millis(25 << attempt)).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    // What `st clients` lists for this stui: its name and build, as reported.
    st3_client::set_client_name(version::client_name());
    let args = std::env::args().collect::<Vec<_>>();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-V"))
    {
        println!("stui {}", version::display_version());
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!(
            "stui [--client | --local] [--space NAME | --classic]\nstui pair MEMBER_URL PAIRING_ID --fingerprint SHA256 [--allow-public-http] (or explicitly --unpinned)\n\nPairing reads the single-use code privately from the terminal (or stdin).\nPaired devices use the network automatically; --local selects the local daemon.\n--client requires a paired device. --demo opens invented data.\nstui opens spaces: splits with their own tabs, Ctrl+K to open anything and Ctrl+S for\nthe sidebar; --space NAME opens that space. --classic keeps the old layout for now; it\nis going away. --version names this build."
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|arg| arg == "pair") {
        anyhow::ensure!(
            args.len() >= 4,
            "Usage: stui pair MEMBER_URL PAIRING_ID --fingerprint SHA256 [--allow-public-http] (or explicitly --unpinned)"
        );
        let mut options = st3_client::device::CompletionOptions::default();
        let mut tail = args[4..].iter();
        while let Some(flag) = tail.next() {
            match flag.as_str() {
                "--allow-public-http" => options.allow_public_http = true,
                "--unpinned" => options.unpinned = true,
                "--fingerprint" if options.fingerprint.is_none() => {
                    options.fingerprint = Some(tail.next().ok_or_else(|| {
                        anyhow::anyhow!(
                            "--fingerprint needs the fingerprint from the trusted machine"
                        )
                    })?)
                }
                _ => anyhow::bail!(
                    "Usage: stui pair MEMBER_URL PAIRING_ID --fingerprint SHA256 [--allow-public-http] (or explicitly --unpinned)"
                ),
            }
        }
        options.validate()?;
        let path = connection::profile_path()?;
        let code = connection::read_code()?;
        let runtime = tokio::runtime::Runtime::new()?;
        let person = runtime.block_on(connection::pair(
            &path,
            &args[2],
            &args[3],
            &code,
            options.allow_public_http,
            options.fingerprint,
            options.unpinned,
        ))?;
        println!("Paired as {person}. Run stui to connect; no local daemon is needed.");
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--demo") {
        return ui::run_demo(&args);
    }
    if !io::stdout().is_terminal() {
        anyhow::bail!("stui needs an interactive terminal");
    }
    let stopping = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, stopping.clone())?;
    }
    let local = args.iter().any(|arg| arg == "--local");
    let client_only = args.iter().any(|arg| arg == "--client");
    anyhow::ensure!(!(local && client_only), "Choose --client or --local");
    let profile = if local {
        None
    } else if std::env::var_os("XDG_CONFIG_HOME").is_none() && std::env::var_os("HOME").is_none() {
        None
    } else {
        connection::profile_path().and_then(|path| connection::Profile::load(&path))?
    };
    anyhow::ensure!(
        !client_only || profile.is_some(),
        "Run stui pair MEMBER_URL PAIRING_ID first"
    );
    let person = match &profile {
        Some(profile) => Some(profile.person()?.to_owned()),
        None => configured_person()?,
    };
    anyhow::ensure!(
        person
            .as_deref()
            .is_some_and(|person| person.starts_with("person/") && person.len() > 7),
        "stui needs ST3_PERSON=person/NAME or person = \"person/NAME\" in the st config"
    );
    let (clients, cache_path) = match &profile {
        Some(profile) => {
            // Scope cached snapshots to this device grant, including every known member.
            let identity = profile
                .devices
                .iter()
                .map(|device| format!("{}:{}", device.endpoint, device.session.device_id))
                .collect::<Vec<_>>()
                .join("|");
            (
                profile.clients()?,
                cache::path(
                    std::path::Path::new(&identity),
                    person.as_deref().unwrap_or_default(),
                ),
            )
        }
        None => {
            let path = st3_client::discover_unix_endpoint(
                std::env::var_os("ST3_ENDPOINT").map(PathBuf::from),
            )?;
            let actor = person.as_deref().unwrap_or_default();
            (
                vec![Client::unix_as(&path, actor)],
                cache::path(&path, actor),
            )
        }
    };
    let client = clients[0].clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let cached = cache_path
        .as_deref()
        .zip(person.as_deref())
        .and_then(|(path, actor)| cache::load(path, actor));
    let (updates, incoming) = mpsc::channel::<feed::Update>();
    let (commands, command_receiver) = tokio::sync::mpsc::unbounded_channel();
    runtime.spawn(feed::run_members(
        clients,
        profile.is_some(),
        ui::glass_request(&args).is_some(),
        updates,
        command_receiver,
    ));
    ui::live::run(ui::live::Context {
        client,
        runtime,
        incoming,
        commands,
        person: person.unwrap_or_default(),
        cache_path,
        cached,
        glass: ui::glass_request(&args),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn words_answer_a_structured_request_the_way_it_takes_them() {
        let request = |kind: &str,
                       custom: bool,
                       answers: serde_json::Value|
         -> st3_client::StructuredRequest {
            serde_json::from_value(serde_json::json!({
                "version": 1, "type": kind, "question": "Ship it?", "why_person": "You own it.",
                "custom": custom, "answers": answers
            }))
            .unwrap()
        };
        let options = serde_json::json!([
            {"id": "works", "label": "Works", "consequence": "Done."},
            {"id": "broken", "label": "Broken", "consequence": "Fixed."}
        ]);
        // Nathan, 2026-10-03: words on a custom choice were sent as a bare summary and refused.
        let custom = request("choice", true, options.clone());
        let answer = person_answer(Some(&custom), None, " found it, works ")
            .unwrap()
            .unwrap();
        assert_eq!(
            (answer.id, answer.text.as_deref()),
            (None, Some("found it, works"))
        );
        // A chosen answer goes by its id.
        let chosen = person_answer(Some(&custom), Some("works".into()), "Works")
            .unwrap()
            .unwrap();
        assert_eq!(chosen.id.as_deref(), Some("works"));
        // Feedback takes words; a free-text ask takes the summary alone.
        let feedback = request("feedback", false, serde_json::json!([]));
        assert!(
            person_answer(Some(&feedback), None, "notes")
                .unwrap()
                .unwrap()
                .text
                .is_some()
        );
        assert!(person_answer(None, None, "notes").unwrap().is_none());
        // A decision takes words as its request for changes; a fixed choice cannot take words.
        let decision = request(
            "decision",
            false,
            serde_json::json!([
                {"id": "land", "label": "Land", "consequence": "Merged.", "outcome": "accept"},
                {"id": "hold", "label": "Hold", "consequence": "Nothing.", "outcome": "decline"},
                {"id": "change", "label": "Change", "consequence": "Revised.", "outcome": "request_changes"}
            ]),
        );
        let chosen = person_answer(Some(&decision), Some("change".into()), "rename it")
            .unwrap()
            .unwrap();
        assert_eq!(chosen.text.as_deref(), Some("rename it"));
        let changes = person_answer(Some(&decision), None, "rename it")
            .unwrap()
            .unwrap();
        assert_eq!(
            (changes.id.as_deref(), changes.text.as_deref()),
            (Some("change"), Some("rename it"))
        );
        assert!(person_answer(Some(&request("choice", false, options)), None, "words").is_err());
    }
    #[test]
    fn regression_person_falls_back_to_local_config() {
        let dir = std::env::temp_dir().join(format!("stui-person-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "person = \"person/alex\"\n").unwrap();
        assert_eq!(
            person_from_config(&path).unwrap().as_deref(),
            Some("person/alex")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn regression_ready_work_and_agent_observation_show_age() {
        assert!(age_label("2026-09-24T11:56:00Z", "2026-09-25T08:56:00Z").contains("21h"));
    }
    #[test]
    fn regression_mission_labels_are_unique() {
        let first: st3_client::Resource = serde_json::from_str(r#"{"kind":"mission","id":"mission/a/issue-triage","revision":"a","updated_at":"2026-09-25T08:00:00Z","title":"Issue Triage","state":"running","mission_revision":"a"}"#).unwrap();
        let second: st3_client::Resource = serde_json::from_str(r#"{"kind":"mission","id":"mission/b/issue-triage","revision":"b","updated_at":"2026-09-25T08:00:00Z","title":"Issue Triage","state":"running","mission_revision":"b"}"#).unwrap();
        let (st3_client::Resource::Mission(a), st3_client::Resource::Mission(b)) = (first, second)
        else {
            panic!()
        };
        assert_eq!(mission_display_label(&a), "a · Issue Triage");
        assert_eq!(mission_display_label(&b), "b · Issue Triage");
    }

    #[test]
    fn regression_now_action_has_a_useful_label_and_key() {
        assert_eq!(action_label("work.done"), "Complete step [c]");
    }

    #[test]
    fn terminal_key_encoding() {
        assert_eq!(
            key_input(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)).as_deref(),
            Some("C-x")
        );
    }

    // Exercise the real key handler and inspect the actions sent to a fake daemon.
    fn terminal_test_client() -> (Client, std::thread::JoinHandle<serde_json::Value>) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::os::unix::net::UnixListener;

        let path = std::env::temp_dir().join(format!("stui-{}.sock", uuid::Uuid::now_v7()));
        let listener = UnixListener::bind(&path).unwrap();
        let client = Client::unix_as(&path, "person/test");
        let server = std::thread::spawn(move || {
            let mut action = serde_json::Value::Null;
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let post = line.starts_with("POST ");
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let mut response: serde_json::Value = serde_json::from_str(include_str!(
                    "../../../docs/st3/client-v0/fixtures/terminal-screen.json"
                ))
                .unwrap();
                if post {
                    action = serde_json::from_slice(&body).unwrap();
                    response["value"] = serde_json::json!({
                        "kind": "action-result", "action_id": action["id"],
                        "operation_id": "operation/test", "status": "completed",
                        "affected_ids": [], "snapshot_id": response["snapshot"]["id"]
                    });
                }
                let body = response.to_string();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
            std::fs::remove_file(path).unwrap();
            action
        });
        (client, server)
    }

    #[test]
    fn terminal_fence_uses_owner_sequence_for_relayed_screen() {
        let mut screen: st3_client::Envelope<st3_client::TerminalScreen> = serde_json::from_str(
            include_str!("../../../docs/st3/client-v0/fixtures/terminal-screen.json"),
        )
        .unwrap();
        screen.snapshot.store_index = 10;
        screen.value.next_sequence = 42;
        let incarnation = screen.value.runtime_incarnation.clone();
        let fence = terminal_screen_fence(&screen, &incarnation).unwrap();
        assert_eq!(fence.terminal_sequence, Some(42));
        assert_eq!(fence.snapshot_id, screen.snapshot.id);
    }

    #[test]
    fn omp_agent_label_names_the_seat_and_driver() {
        let agent: st3_client::Agent = serde_json::from_str(r#"{"kind":"agent","id":"agent/example/pty-rust/omp","revision":"one","updated_at":"2026-09-25T08:00:00Z","name":"fleet/pty-rust/omp","state":"running","reachability":"local","runtime_ids":[],"under":[]}"#).unwrap();
        assert_eq!(agent_label(&agent), "PTY Rust · OMP");
    }

    #[test]
    fn a_seat_named_after_its_host_is_labelled_without_the_host() {
        let agent = |name: &str, host: &str| -> st3_client::Agent {
            serde_json::from_value(serde_json::json!({"kind":"agent","id":format!("agent/{name}"),"revision":"one","updated_at":"2026-10-04T08:00:00Z","name":name,"state":"running","reachability":"local","runtime_ids":[],"under":[],"host_id":host})).unwrap()
        };
        assert_eq!(agent_label(&agent("harbor.image-sorter", "host/harbor")), "Image Sorter");
        assert_eq!(agent_label(&agent("Harbor.image-sorter", "host/harbor")), "Image Sorter");
        // A dot that is not its host's stays part of the name.
        assert_eq!(agent_label(&agent("v2.parser", "host/harbor")), "V2.parser");
    }

    /// A runtime that starts nothing: the gate in these tests waits only on a person.
    struct NoRuntime;

    impl st3::reconcile::RuntimeControl for NoRuntime {
        fn snapshot_ptys(&self) -> anyhow::Result<Vec<st3::reconcile::RuntimeObservation>> {
            Ok(Vec::new())
        }
        fn observe_exec(
            &self,
            _: &str,
        ) -> anyhow::Result<Option<st3::reconcile::RuntimeObservation>> {
            Ok(None)
        }
        fn start(&self, _: &st3::model::MemberSpec) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }
        fn kill(&self, _: &str, _: bool, _: Option<&str>) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove(&self, _: &str, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn screen(&self, _: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn read_exec_log(&self, _: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
    }

    /// st in this process, running a release whose one step waits on person/avery's approval.
    struct WaitingGate {
        _root: tempfile::TempDir,
        store: Arc<st3::store::Store>,
        reconciler: st3::reconcile::Reconciler<NoRuntime>,
        client: Client,
        step: String,
        server: tokio::task::JoinHandle<()>,
        state: st3::api::AppState,
        socket: std::path::PathBuf,
    }

    impl WaitingGate {
        async fn start() -> Self {
            let gate = Self::serve(
                r#"version 2
mission "release" state="ready" {
  goal "Ship the release once a person approves it."
  step "approve" {
    agentless
    title "Approve the release"
    gate "accept" type="human" {
      reviewer "person/avery"
      question "Ship this release?"
    }
  }
}
"#,
            )
            .await;
            gate.reconcile_until("the gate asked", |gate| gate.request().is_some());
            gate
        }

        /// st serving a run of the `release` mission in `source`, before any reconcile pass.
        async fn serve(source: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let node = "stui-gate";
            let store =
                Arc::new(st3::store::Store::open(&root.path().join("graph.db"), node).unwrap());
            let intent = st3::graph::parse_intent(source, node).unwrap();
            let planned = store
                .mission(
                    &intent,
                    st3::model::IntentInput {
                        kdl: source.into(),
                        source_name: None,
                    },
                )
                .unwrap();
            store
                .apply(&intent, &planned.subject_tokens, "release")
                .unwrap();
            let run = store
                .create_mission_run(&st3::model::MissionRunRequest {
                    mission: "release".into(),
                    revision: None,
                    workspace: "/tmp".into(),
                    requester: Some("person/avery".into()),
                    mode: Some("run".into()),
                    inputs: BTreeMap::new(),
                    idempotency_key: "release-run".into(),
                })
                .unwrap();
            let state = st3::api::AppState {
                store: store.clone(),
                notify: Arc::new(tokio::sync::Notify::new()),
                event_notify: tokio::sync::watch::channel(0_u64).0,
                node: node.into(),
                state_dir: root.path().into(),
                pty_root: root.path().join("pty"),
                pty_binary: root.path().join("unused-pty"),
                fleet_id: None,
                configured_peers: vec![],
                client_relay: None,
                native_session_home: None,
                planner_default: st3::model::PlannerSpec::default(),
            };
            let socket = root.path().join("st3.sock");
            let server_socket = socket.clone();
            let served_state = state.clone();
            let server = tokio::spawn(async move {
                st3::api::serve_unix(&server_socket, st3::api::router(served_state))
                    .await
                    .unwrap();
            });
            for _ in 0..200 {
                if socket.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Self {
                reconciler: st3::reconcile::Reconciler::new(
                    store.clone(),
                    Arc::new(NoRuntime),
                    node.into(),
                    Arc::new(tokio::sync::Notify::new()),
                ),
                client: Client::unix_as(&socket, "person/avery"),
                step: run.steps[0].subject.clone(),
                store,
                server,
                _root: root,
                state,
                socket,
            }
        }

        async fn restart(&mut self) {
            self.server.abort();
            let _ = (&mut self.server).await;
            std::fs::remove_file(&self.socket).unwrap();
            self.store = Arc::new(
                st3::store::Store::open(&self._root.path().join("graph.db"), "stui-gate").unwrap(),
            );
            self.state.store = self.store.clone();
            self.state.notify = Arc::new(tokio::sync::Notify::new());
            self.state.event_notify = tokio::sync::watch::channel(0_u64).0;
            self.reconciler = st3::reconcile::Reconciler::new(
                self.store.clone(),
                Arc::new(NoRuntime),
                "stui-gate".into(),
                self.state.notify.clone(),
            );
            let state = self.state.clone();
            let socket = self.socket.clone();
            self.server = tokio::spawn(async move {
                st3::api::serve_unix(&socket, st3::api::router(state))
                    .await
                    .unwrap();
            });
            for _ in 0..200 {
                if self.socket.exists() {
                    self.client.capabilities().await.unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("restarted stui fixture did not listen");
        }

        fn reconcile_until(&self, what: &str, done: impl Fn(&Self) -> bool) {
            for _ in 0..30 {
                self.reconciler.reconcile_once().unwrap();
                if done(self) {
                    return;
                }
            }
            panic!("{what} did not happen within 30 reconcile passes");
        }

        fn request(&self) -> Option<String> {
            self.store
                .gate_request_for_owner(&self.step)
                .unwrap()
                .map(|request| request.id)
        }

        fn status(&self) -> String {
            self.store.step_run(&self.step).unwrap().unwrap().status
        }

        /// The person's card for the gate, as stui lists it.
        async fn card(&self) -> Option<st3_client::Attention> {
            self.client
                .attention_list(None, Some(50), false)
                .await
                .unwrap()
                .value
                .items
                .into_iter()
                .find_map(|item| match item {
                    Resource::Attention(card) if card.source_id == self.step => Some(card),
                    _ => None,
                })
        }

        /// The step fails and is retried, and st asks the gate again for the new attempt: the
        /// card the person has open names a request the gate no longer waits on.
        fn ask_again(&self) {
            let asked = self.request();
            assert!(
                self.store
                    .set_step_state(&self.step, "failed", Some("the build broke"))
                    .unwrap()
            );
            assert!(self.store.retry_step(&self.step, "rebuilt", 0).unwrap());
            self.reconcile_until("the gate asked again", |gate| gate.request() != asked);
        }
    }

    impl Drop for WaitingGate {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn launch_review_uses_its_public_launch_and_selected_variant_after_restart() {
        for action in ["launch.approve", "launch.cancel"] {
            let mut gate = WaitingGate::start().await;
            let snapshot = gate.client.capabilities().await.unwrap().snapshot.id;
            gate.client
                .launch_create(
                    "action/ui-launch",
                    "ui-create-launch",
                    Fence {
                        snapshot_id: snapshot,
                        ..Fence::default()
                    },
                    st3_client::LaunchCreateParameters {
                        title: "Copper launch".into(),
                        request: "Prepare the copper proof".into(),
                        target: st3_client::LaunchTarget::NewMission {
                            mission_id: "mission/example/copper".into(),
                            workspace: gate._root.path().display().to_string(),
                        },
                        provider: None,
                        model: None,
                        effort: None,
                    },
                )
                .await
                .unwrap();
            let session = gate.store.planning_sessions(true).unwrap().pop().unwrap();
            let transport = st3::client::Client::unix_as(&gate.socket, "person/avery").unwrap();
            for variant in ["default", "alternate"] {
                let _: serde_json::Value = transport.post(&format!("/v1/launches/{}/variants/{variant}/submit", session.id), &st3::model::PlanningCandidateSubmitRequest {
                    actor: session.planner.clone(), markdown: b"Copper proof".to_vec(),
                    kdl: b"version 2\nmission \"example/copper\" state=\"ready\" { goal \"Record copper proof.\"; step \"proof\" { agentless } }\n".to_vec(), idempotency_key: format!("ui-submit-{variant}"),
                }).await.unwrap();
            }
            let card = gate
                .client
                .attention_list(None, Some(100), false)
                .await
                .unwrap()
                .value
                .items
                .iter()
                .find_map(|item| match item {
                    Resource::Attention(card) if card.source_id == session.subject => {
                        Some(card.clone())
                    }
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                card.variant_id.as_deref(),
                Some(format!("launch-variant/{}/default", session.id).as_str())
            );
            gate.restart().await;
            attention_action(
                &gate.client,
                "person/avery",
                &card.header.id,
                Some(&card),
                action,
                None,
                None,
            )
            .await
            .unwrap();
            gate.restart().await;
            let result = gate.store.planning_session(&session.id).unwrap().unwrap();
            assert_eq!(
                result.status,
                if action == "launch.approve" {
                    "approved"
                } else {
                    "cancelled"
                }
            );
            assert_eq!(result.candidate.unwrap().variant, "default");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approving_a_gate_asked_again_acts_once_on_its_current_card() {
        let mut gate = WaitingGate::start().await;
        let seen = gate.card().await.expect("the gate has a card");
        gate.ask_again();
        gate.restart().await;
        let current = gate.card().await.expect("the gate was asked again");
        assert_ne!(current.header.id, seen.header.id);

        let outcome = attention_action(
            &gate.client,
            "person/avery",
            &seen.header.id,
            Some(&seen),
            "review.approve",
            None,
            None,
        )
        .await
        .expect("the approval did not reach the current card");
        assert!(outcome.starts_with("Approve"), "{outcome}");
        gate.reconcile_until("the step completed", |gate| gate.status() == "completed");
    }

    /// st asked the gate again with something the person has not read: the approval is not
    /// carried over to it, and the card is shown again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_review_that_changed_since_it_was_read_is_shown_again_not_answered() {
        let gate = WaitingGate::start().await;
        let mut seen = gate.card().await.expect("the gate has a card");
        gate.ask_again();
        seen.detail = "what the person read before the step changed".into();
        let error = attention_action(
            &gate.client,
            "person/avery",
            &seen.header.id,
            Some(&seen),
            "review.approve",
            None,
            None,
        )
        .await
        .expect_err("a changed review was answered unread");
        assert!(
            error.to_string().contains("changed since you read it"),
            "{error}"
        );
        for _ in 0..3 {
            gate.reconciler.reconcile_once().unwrap();
        }
        assert_ne!(gate.status(), "completed");
        assert!(gate.card().await.is_some(), "the review is still asked");
    }

    /// The card was read, then st asked the gate again before the approval arrived: st refuses
    /// the stale fence, and the approval goes once more to the card st shows now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approving_on_a_stale_fence_retries_once_on_the_current_card() {
        let gate = WaitingGate::start().await;
        let seen = gate.card().await.expect("the gate has a card");
        let read = gate.client.attention_get(&seen.header.id).await.unwrap();
        gate.ask_again();

        let outcome = act_on_attention(
            &gate.client,
            "person/avery",
            Ok(read.clone()),
            None,
            "review.approve",
            Some("ship it".into()),
            None,
        )
        .await
        .expect("the approval did not reach the current card");
        assert!(outcome.starts_with("Approve"), "{outcome}");
        gate.reconcile_until("the step completed", |gate| gate.status() == "completed");

        // The same stale card again: st no longer asks, and the person reads why.
        let error = act_on_attention(
            &gate.client,
            "person/avery",
            Ok(read),
            None,
            "review.approve",
            None,
            None,
        )
        .await
        .expect_err("a spent card takes no answer");
        assert!(error.to_string().contains("no longer asked"), "{error}");
    }

    /// A feedback gate's card offers request-changes, and sending it back from stui gives the
    /// worker a new attempt with the person's notes in its goals.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sending_back_a_feedback_gate_requests_changes() {
        let gate = WaitingGate::serve(
            r#"version 2
agent "worker" { workspace "/tmp"; command "true" }
mission "release" state="ready" {
  goal "Write the release notes."
  step "draft" {
    assigned-to "agent/worker"
    goal "Draft the release notes."
    gate "review" type="human" mode="feedback" { reviewer "person/avery" }
  }
}
"#,
        )
        .await;
        let worker = gate
            .store
            .step_run(&gate.step)
            .unwrap()
            .unwrap()
            .assigned_to
            .unwrap();
        gate.store
            .set_step_state(&gate.step, "ready", None)
            .unwrap();
        for action in ["claim", "complete"] {
            gate.store
                .work_action(
                    &gate.step,
                    action,
                    &st3::model::WorkRequest {
                        actor: Some(worker.clone()),
                        incarnation: Some("worker-one".into()),
                        summary: Some("Drafted".into()),
                        reason: None,
                        evidence: Vec::new(),
                        idempotency_key: format!("draft-{action}"),
                    },
                )
                .unwrap();
        }
        gate.reconcile_until("the gate asked", |gate| gate.request().is_some());
        let seen = gate.card().await.expect("the gate has a card");
        assert_eq!(seen.actions, ["review.approve", "review.request-changes"]);

        attention_action(
            &gate.client,
            "person/avery",
            &seen.header.id,
            Some(&seen),
            "review.request-changes",
            Some("Add the missing source.".into()),
            None,
        )
        .await
        .expect("the notes did not go back to the worker");
        gate.reconcile_until("the step started again", |gate| {
            gate.store.step_run(&gate.step).unwrap().unwrap().attempt == 2
        });
        let step = gate.store.step_run(&gate.step).unwrap().unwrap();
        assert!(
            step.goals
                .iter()
                .any(|goal| goal.contains("Add the missing source.")),
            "{:?}",
            step.goals
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approving_a_gate_already_answered_says_so() {
        let gate = WaitingGate::start().await;
        let seen = gate.card().await.expect("the gate has a card");
        attention_action(
            &gate.client,
            "person/avery",
            &seen.header.id,
            Some(&seen),
            "review.approve",
            None,
            None,
        )
        .await
        .unwrap();
        gate.reconcile_until("the step completed", |gate| gate.status() == "completed");

        let error = attention_action(
            &gate.client,
            "person/avery",
            &seen.header.id,
            Some(&seen),
            "review.reject",
            Some("no".into()),
            None,
        )
        .await
        .expect_err("an answered gate takes no second answer");
        let error = error.to_string();
        assert!(
            error.contains("no longer asked") && error.contains("completed"),
            "{error}"
        );
    }
}

fn custom_form_hint(form: Option<&serde_json::Value>) -> String {
    let Some(fields) = form.and_then(|f| f["fields"].as_object()) else {
        return "Open the source to reply.".into();
    };
    fields
        .iter()
        .map(|(name, f)| {
            let choices = f["values"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" / ")
                })
                .unwrap_or_default();
            let required = if f["required"] == true {
                "required"
            } else {
                "optional"
            };
            format!(
                "{} ({}{})",
                name,
                required,
                if choices.is_empty() {
                    String::new()
                } else {
                    format!(", {choices}")
                }
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
        + ". Enter a value for the single required field, or a JSON object for several fields."
}
fn custom_form_input(
    form: Option<&serde_json::Value>,
    text: &str,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>> {
    if let Ok(fields) =
        serde_json::from_str::<std::collections::BTreeMap<String, serde_json::Value>>(text)
    {
        return Ok(fields);
    }
    let fields = form
        .and_then(|f| f["fields"].as_object())
        .ok_or_else(|| anyhow::anyhow!("this source has no reply form"))?;
    let required = fields
        .iter()
        .filter(|(_, f)| f["required"] == true)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        required.len() == 1,
        "reply with a JSON object containing the displayed fields"
    );
    let (name, f) = required[0];
    let value = if f["value_type"] == "string" || f["value_type"] == "subject-reference" {
        serde_json::json!(text.trim())
    } else {
        serde_json::from_str(text)
            .map_err(|_| anyhow::anyhow!("enter a value of the displayed field's type"))?
    };
    Ok(std::collections::BTreeMap::from([(name.clone(), value)]))
}

#[cfg(test)]
mod custom_form_tests {
    use super::*;
    #[test]
    fn custom_forms_preserve_declared_values_multiple_choices_and_reframes() {
        let form = serde_json::json!({"fields":{"selection":{"required":true,"value_type":"string","values":["keep","discard"]},"text":{"value_type":"string"}}});
        assert_eq!(
            custom_form_input(Some(&form), "keep").unwrap()["selection"],
            "keep"
        );
        assert!(custom_form_hint(Some(&form)).contains("keep / discard"));
        let multi = serde_json::json!({"fields":{"selection":{"required":true,"value_type":"array"},"text":{"value_type":"string"}}});
        let fields = custom_form_input(
            Some(&multi),
            r#"{"selection":[],"text":"Reframe the question"}"#,
        )
        .unwrap();
        assert_eq!(fields["selection"], serde_json::json!([]));
        assert_eq!(fields["text"], "Reframe the question");
        assert_eq!(
            custom_form_input(Some(&multi), r#"["keep","discard"]"#).unwrap()["selection"],
            serde_json::json!(["keep", "discard"])
        );
        assert!(custom_form_input(Some(&multi), "keep").is_err());
    }
}
