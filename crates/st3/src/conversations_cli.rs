use super::*;
// The same print macros as main.rs: CLI output goes through st3's writer.
use st3::{cli_print as print, cli_println as println};

fn print_timeline_page(
    response: &ClientEnvelope<ClientTimelinePage>,
    json_output: bool,
) -> Result<()> {
    if json_output {
        return print_value(response, true);
    }
    use std::fmt::Write as _;
    let mut output = timeline_entries_text(&response.value.session_id, &response.value.items);
    if response.value.page.has_more
        && let Some(cursor) = &response.value.page.next_cursor
    {
        let _ = writeln!(
            output,
            "\nOlder entries: st conversations timeline {} --cursor {}",
            response.value.session_id,
            shell_argument(cursor)
        );
    }
    print!("{output}");
    Ok(())
}

/// A conversation page as stui draws it, through the shared renderer: in colour on a terminal
/// (unless `NO_COLOR` is set), plain text otherwise.
fn print_conversation_page(
    response: &ClientEnvelope<ClientTimelinePage>,
    simple: bool,
) -> Result<()> {
    let stdout = cli_output::stdout();
    let color = stdout.is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let width = terminal_columns().unwrap_or(100).clamp(40, 160);
    let mut output = conversation_text(
        &response.value.session_id,
        &response.value.items,
        width,
        color,
        if simple {
            st3_conversation_ui::Density::Simple
        } else {
            st3_conversation_ui::Density::Full
        },
    );
    if response.value.page.has_more
        && let Some(cursor) = &response.value.page.next_cursor
    {
        output.push_str(&format!(
            "\nOlder entries: st conversations timeline {} --cursor {}\n",
            response.value.session_id,
            shell_argument(cursor)
        ));
    }
    print!("{output}");
    Ok(())
}

fn print_follow_entries(
    session_id: &str,
    changed: Vec<ClientTimelineEntry>,
    json_output: bool,
) -> Result<()> {
    if changed.is_empty() {
        return Ok(());
    }
    if json_output {
        for entry in changed {
            println!("{}", serde_json::to_string(&entry)?);
        }
        return Ok(());
    }
    print!("{}", timeline_entries_text(session_id, &changed));
    Ok(())
}

/// Follow a conversation the way every client sees it: st joins the transcript and the Small
/// Talk and pushes each change on the collection socket.
async fn follow_conversation(
    client: &GeneratedClient,
    target: &str,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let mut stream = client.collection_stream().await?;
    stream
        .subscribe_conversation("conversation", target)
        .await?;
    let mut seen = BTreeMap::new();
    let mut stalled: Option<String> = None;
    loop {
        if cli_output::closed() {
            return Ok(());
        }
        match stream.next_event().await? {
            None => anyhow::bail!("st closed the conversation stream"),
            Some(st3_client::CollectionEvent::Conversation {
                session_id,
                replace,
                items,
                ..
            }) => {
                stalled = None;
                // The first page shows its newest `limit` entries; later pages only what changed.
                let items = if replace && seen.is_empty() {
                    items[items.len().saturating_sub(limit)..].to_vec()
                } else {
                    items
                };
                print_follow_entries(
                    &session_id,
                    unseen_timeline_entries(&items, &mut seen),
                    json_output,
                )?;
            }
            Some(st3_client::CollectionEvent::Error { message, .. }) => {
                anyhow::bail!("st could not show this conversation: {message}")
            }
            // st retries on its own; say why once, so a quiet follow is not read as a quiet agent.
            Some(st3_client::CollectionEvent::Resync {
                code,
                message: Some(message),
                ..
            }) => {
                let message = st3_client::plain_message(code.as_ref(), &message);
                if stalled.as_deref() != Some(message.as_str()) {
                    eprintln!("st: {message} · trying again");
                    stalled = Some(message);
                }
            }
            Some(_) => {}
        }
    }
}

pub(super) async fn upload_attachments(
    endpoint: &Endpoint,
    actor: &str,
    files: &[PathBuf],
) -> Result<Vec<st3::model::AttachmentInput>> {
    let mut attachments = Vec::new();
    for file in files {
        attachments.push(upload_attachment(endpoint, actor, file, None).await?);
    }
    Ok(attachments)
}

pub(super) async fn run_message(
    client: &Client,
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    command: MessageCommand,
    json_output: bool,
) -> Result<()> {
    sync_message_projection(client).await?;
    match command {
        MessageCommand::Send(args) => {
            let attachments = if args.attach.is_empty() {
                Vec::new()
            } else {
                let from = blob_actor(Some(args.from.clone()), None)?;
                upload_attachments(endpoint, &from, &args.attach).await?
            };
            let Some(receipt) = send_message(client, args, attachments).await? else {
                return Ok(());
            };
            // The message has landed. A projection that misses it catches up on the next command;
            // failing here would invite a resend of a message that was sent.
            if let Err(error) = sync_message_projection(client).await {
                eprintln!(
                    "st: {} was sent (idempotency key {}), but the message projection was not refreshed: {}",
                    receipt.message.subject,
                    receipt.idempotency_key,
                    plain_error(&error)
                );
            }
            print_message_receipt(&receipt, json_output)
        }
        MessageCommand::Status(args) => {
            if let Some(key) = args.idempotency_key {
                return print_message_key_status(client, &key, json_output).await;
            }
            let reference = args
                .reference
                .context("conversations status needs a message or --idempotency-key")?;
            let value = message_delivery(client, &reference).await?;
            if json_output {
                print_value(&value, true)
            } else {
                print_message_delivery(&value);
                Ok(())
            }
        }
        MessageCommand::Ls(args) => {
            let identity = message_list_identity(
                args.identity.or(args.actor),
                std::env::var("ST_AGENT").ok(),
            )?;
            reject_foreign_agent_actor(&identity)?;
            let sender = args.sender.map(|sender| normalize_message_subject(&sender));
            let mut count = 0_u64;
            let mut first = true;
            let mut rows = Vec::new();
            if json_output && !args.count {
                print!("[");
            }
            let filters = MessagePageFilters {
                from: sender.as_deref(),
                subject: args.subject.as_deref(),
                subject_prefix: args.subject_prefix.as_deref(),
            };
            let mut cursor = None;
            loop {
                let page = message_page_filtered(
                    client,
                    Some(&identity),
                    args.archive,
                    cursor.as_deref(),
                    None,
                    filters,
                )
                .await?;
                for message in page.items {
                    // Older daemons ignore the query filters; keep the same local result there.
                    if sender
                        .as_deref()
                        .is_some_and(|sender| sender != message.from)
                        || args
                            .subject
                            .as_deref()
                            .is_some_and(|subject| message.title.as_deref() != Some(subject))
                        || args.subject_prefix.as_deref().is_some_and(|prefix| {
                            !message
                                .title
                                .as_deref()
                                .is_some_and(|title| title.starts_with(prefix))
                        })
                    {
                        continue;
                    }
                    count += 1;
                    if args.count {
                        continue;
                    }
                    if json_output {
                        if !first {
                            print!(",");
                        }
                        print!("{}", serde_json::to_string(&message)?);
                        first = false;
                    } else {
                        rows.push(format!(
                            "{}\t{}\t{}\t{}",
                            message.subject,
                            message.status,
                            message.from,
                            message.title.as_deref().unwrap_or("message")
                        ));
                    }
                }
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            if args.count {
                println!("{count}");
            } else if json_output {
                println!("]");
            } else {
                print!(
                    "{}",
                    render_mailbox(&identity, sender.as_deref(), args.archive, &rows)
                );
            }
            Ok(())
        }
        MessageCommand::Read(args) => {
            let actor = args
                .actor
                .context("message read needs explicit --as to record its lifecycle")?;
            reject_foreign_agent_actor(&actor)?;
            let mut messages = Vec::with_capacity(args.references.len());
            for reference in args.references {
                messages.push(
                    read_message_after_lifecycle(client, &reference, &actor, args.archive).await?,
                );
            }
            if json_output {
                let mut shown = Vec::with_capacity(messages.len());
                for message in &messages {
                    let mut value = serde_json::to_value(message)?;
                    if message.body_ref.is_some() {
                        // `content` is only the preview; `text` is the whole message.
                        value["text"] = st3::message_body::full_text(client, message).await?.into();
                    }
                    if let Some(provenance) = message_provenance(client, &message.subject).await {
                        value["provenance"] = provenance;
                    }
                    shown.push(value);
                }
                if shown.len() == 1 {
                    print_value(&shown[0], true)?;
                } else {
                    print_value(&shown, true)?;
                }
            } else {
                for (index, message) in messages.iter().enumerate() {
                    if index > 0 && !args.raw {
                        println!("\n---\n");
                    }
                    let text = st3::message_body::full_text(client, message).await?;
                    if args.raw {
                        print!("{text}");
                        if index + 1 < messages.len() && !text.ends_with('\n') {
                            println!();
                        }
                    } else {
                        println!("Message: {}", message.subject);
                        println!("From: {}", message.from);
                        if let Some(provenance) = message_provenance(client, &message.subject).await {
                            println!("Signed: {}", provenance_line(&provenance));
                        }
                        println!("To: {}", message.to);
                        if let Some(title) = &message.title {
                            println!("Subject: {title}");
                        }
                        println!();
                        println!("{text}");
                        for attachment in &message.attachments {
                            println!(
                                "Attachment: blob/{} ({}, {} bytes{}); read it with `st blobs get blob/{} --message {} -o FILE`",
                                attachment.sha256,
                                attachment.media_type,
                                attachment.size,
                                attachment
                                    .name
                                    .as_deref()
                                    .map(|name| format!(", {name}"))
                                    .unwrap_or_default(),
                                attachment.sha256,
                                message.subject,
                            );
                        }
                    }
                }
            }
            sync_message_projection(client).await?;
            Ok(())
        }
        MessageCommand::Reply(args) => {
            let original = read_message(client, &args.reference).await?;
            let recipient = message_reply_recipient(&original, &args.from)?;
            let attachments = if args.attach.is_empty() {
                Vec::new()
            } else {
                let from = blob_actor(Some(args.from.clone()), None)?;
                upload_attachments(endpoint, &from, &args.attach).await?
            };
            let receipt = send_message(
                client,
                MessageSendArgs {
                    to: recipient,
                    attach: Vec::new(),
                    body: args.body,
                    subject: args
                        .subject
                        .or(original.title.map(|title| format!("Re: {title}"))),
                    in_reply_to: Some(original.subject),
                    tags: Vec::new(),
                    from: args.from,
                    wake: args.wake,
                    print_kdl: args.print_kdl,
                    idempotency_key: args.idempotency_key,
                },
                attachments,
            )
            .await?;
            let Some(receipt) = receipt else {
                return Ok(());
            };
            print_message_receipt(&receipt, json_output)
        }
        MessageCommand::Wait(args) => {
            let actor = message_list_identity(args.actor, std::env::var("ST_AGENT").ok())?;
            reject_foreign_agent_actor(&actor)?;
            let sender = normalize_message_subject(&args.sender);
            let timeout = parse_timeout(&args.timeout)?;
            anyhow::ensure!(
                timeout <= MAX_REPLY_WAIT,
                "a wait lasts at most 10m; issue it again to wait longer"
            );
            let after = match args.after.as_deref() {
                Some(reference) => Some(read_message(client, reference).await?.created_index),
                None => None,
            };
            let reply = wait_for_reply(client, &actor, &sender, after, timeout).await?;
            let reply = match reply {
                Some(message) => {
                    Some(read_message_after_lifecycle(client, &message.subject, &actor, false).await?)
                }
                None => None,
            };
            sync_message_projection(client).await?;
            print_reply_wait(reply.as_ref(), json_output)
        }
        MessageCommand::Cleanup { all, actor, older_than, dry_run } => {
            if let Some(actor) = actor.as_deref() {
                reject_foreign_agent_actor(actor)?;
            }
            let older_than_ms = st3::graph::parse_duration(&older_than, true)?;
            let result: Value = client
                .post("/v1/messages/cleanup", &json!({
                    "older_than_ms": older_than_ms, "to": actor, "all": all, "dry_run": dry_run,
                }))
                .await?;
            sync_message_projection(client).await?;
            if json_output {
                print_value(&result, true)
            } else {
                println!("{} {} unread messages older than {}", if dry_run { "Would archive" } else { "Archived" }, result["count"], older_than);
                if dry_run {
                    for id in result["messages"].as_array().into_iter().flatten() {
                        if let Some(id) = id.as_str() {
                            println!("{id}");
                        }
                    }
                }
                Ok(())
            }
        }
        MessageCommand::Archive(args) => {
            let delegation = args.delegation.proof(args.episode.as_deref())?;
            let actor = args
                .actor
                .context("message archive needs explicit --as to record its lifecycle")?;
            reject_foreign_agent_actor(&actor)?;
            let mut claims = Vec::with_capacity(args.references.len());
            anyhow::ensure!(
                delegation.is_none() || args.references.len() == 1,
                "delegated archive names one message and episode at a time"
            );
            for reference in args.references {
                if let Some(proof) = &delegation {
                    let reference = normalize_message_reference(&reference);
                    let claim: ClaimRecord = client
                        .post(
                            &format!("/v1/messages/{}/claims", urlencoding::encode(&reference)),
                            &MessageLifecycleRequest {
                                delegation: Some(proof.clone()),
                                lifecycle: "closed".into(),
                                actor: Some(actor.clone()),
                                transport: None,
                                runtime_id: None,
                                evidence: Vec::new(),
                                expected_subject: None,
                                idempotency_key: format!(
                                    "delegated-archive:{reference}:{}:{}",
                                    proof.policy, proof.message
                                ),
                            },
                        )
                        .await?;
                    claims.push(claim);
                    continue;
                }
                let message = read_message(client, &reference).await?;
                accept_message(client, &message, &actor).await?;
                claims.push(close_message(client, &reference, &actor).await?);
            }
            sync_message_projection(client).await?;
            if json_output {
                if claims.len() == 1 {
                    print_value(&claims[0], true)
                } else {
                    print_value(&claims, true)
                }
            } else {
                Ok(())
            }
        }
        MessageCommand::Thread(args) => {
            let selected = read_message(client, &args.reference).await?;
            // Page through the whole history once; the daemon reads every message for each pass.
            let mut messages = Vec::new();
            for_each_message(client, None, true, |message| {
                messages.push(message);
                Ok(())
            })
            .await?;
            let links = messages
                .iter()
                .map(|message| (message.subject.clone(), message.in_reply_to.clone()))
                .collect::<BTreeMap<_, _>>();
            let root = thread_root_from_links(&selected.subject, &links);
            let mut thread = messages
                .into_iter()
                .filter(|message| thread_root_from_links(&message.subject, &links) == root)
                .collect::<Vec<_>>();
            thread.sort_by_key(|message| message.created_index);
            print_value(&thread, json_output)
        }
        MessageCommand::Search {
            text,
            actor,
            agent,
            since,
            cursor,
            limit,
        } => {
            anyhow::ensure!(
                (1..=200).contains(&limit),
                "the search limit must be 1 through 200"
            );
            let response = generated_client(endpoint, actor.as_deref().or(configured_person))?
                .conversation_search(
                    &text,
                    agent.as_deref(),
                    since.as_deref(),
                    cursor.as_deref(),
                    Some(limit),
                )
                .await?;
            if json_output {
                return print_value(&response, true);
            }
            for hit in &response.value.items {
                println!(
                    "{}  {}  {}\n{}\n",
                    hit.timestamp, hit.conversation_id, hit.entry_id, hit.excerpt
                );
            }
            println!(
                "{} matches on {} (indexed {})",
                response.value.items.len(),
                response.value.host_id,
                response.value.indexed_at
            );
            if response.value.refreshing {
                eprintln!("The search index is refreshing; repeat this search for newer text.");
            }
            for source in &response.value.incomplete_sources {
                eprintln!("Incomplete history: {source}");
            }
            if let Some(cursor) = response.value.page.next_cursor {
                println!("Older matches: repeat this search with --cursor {cursor}");
            }
            Ok(())
        }
        MessageCommand::Sessions {
            actor,
            all,
            cursor,
            limit,
        } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the session limit must be 1 through 200"
            );
            let response = generated_client(endpoint, actor.as_deref().or(configured_person))?
                .sessions_list(cursor.as_deref(), Some(limit), all)
                .await?;
            let history = if all { " --all" } else { "" };
            print_product_page(
                "SESSIONS",
                &response,
                json_output,
                &format!("st conversations sessions{history}"),
            )
        }
        MessageCommand::Timeline {
            session,
            actor,
            limit,
            cursor,
            raw,
            simple,
        } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the timeline limit must be 1 through 200"
            );
            let response = generated_client(endpoint, actor.as_deref().or(configured_person))?
                .timeline(&session, cursor.as_deref(), Some(limit))
                .await?;
            if raw || json_output {
                print_timeline_page(&response, json_output)
            } else {
                print_conversation_page(&response, simple)
            }
        }
        MessageCommand::Follow {
            session,
            actor,
            limit,
        } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the timeline limit must be 1 through 200"
            );
            follow_conversation(
                &generated_client(endpoint, actor.as_deref().or(configured_person))?,
                &session,
                limit,
                json_output,
            )
            .await
        }
        MessageCommand::Export { directory } => {
            let mut export = st3::projection::MessageExport::new(&directory)?;
            let mut count = 0_u64;
            for_each_message(client, None, true, |message| {
                export.write(&message)?;
                count += 1;
                Ok(())
            })
            .await?;
            export.finish()?;
            if json_output {
                print_value(&json!({"directory": directory, "messages": count}), true)
            } else {
                println!("exported {} messages to {}", count, directory.display());
                Ok(())
            }
        }
    }
}

async fn message_delivery(client: &Client, reference: &str) -> Result<Value> {
    client
        .get(&format!(
            "/v1/messages/delivery/{}",
            urlencoding::encode(reference)
        ))
        .await
}

fn print_message_delivery(value: &Value) {
    println!(
        "{} · {} → {}",
        value["id"].as_str().unwrap_or("message"),
        value["from"].as_str().unwrap_or("sender"),
        value["to"].as_str().unwrap_or("recipient")
    );
    println!(
        "{} · {}",
        value
            .pointer("/delivery/phase")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        value
            .pointer("/delivery/reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
}

/// Whether a send or reply with this idempotency key landed, and if so its delivery.
async fn print_message_key_status(client: &Client, key: &str, json_output: bool) -> Result<()> {
    let receipt = client
        .sent_message(key)
        .await
        .with_context(|| format!("look up the message sent with idempotency key {key}"))?;
    let Some(receipt) = receipt else {
        if json_output {
            return print_value(&json!({"idempotency_key": key, "landed": false}), true);
        }
        println!(
            "not landed · no message was sent with idempotency key {key}; sending it again sends it once"
        );
        return Ok(());
    };
    let mut value = message_delivery(client, &receipt.message.subject).await?;
    if json_output {
        value["idempotency_key"] = json!(key);
        value["landed"] = json!(true);
        value["sent_at"] = json!(receipt.sent_at);
        return print_value(&value, true);
    }
    println!(
        "landed · {} at {}",
        receipt.message.subject,
        receipt.sent_at.as_deref().unwrap_or("an unknown time")
    );
    print_message_delivery(&value);
    Ok(())
}

/// The earliest message to `actor` from `sender` that arrived after `after`.
fn earliest_reply(messages: Vec<MessageView>, sender: &str, after: u64) -> Option<MessageView> {
    messages
        .into_iter()
        .filter(|message| {
            message.from == sender
                && message.created_index > after
                && matches!(message.status.as_str(), "sent" | "staged" | "delivered")
        })
        .min_by_key(|message| message.created_index)
}

async fn unread_replies(client: &Client, actor: &str, sender: &str, after: u64) -> Result<Option<MessageView>> {
    let mut candidates = Vec::new();
    for_each_message(client, Some(actor), false, |message| {
        candidates.push(message);
        Ok(())
    })
    .await?;
    Ok(earliest_reply(candidates, sender, after))
}

/// The longest one wait holds a tool call and a connection; a longer wait is issued again.
const MAX_REPLY_WAIT: Duration = Duration::from_secs(600);

/// The least time between two reads of the event feed by one wait, so a busy store cannot make a
/// waiting seat poll.
const REPLY_WAIT_PACE: Duration = Duration::from_millis(500);

/// Whether an event may be new mail from `sender` to `actor`. An event whose body names its
/// endpoints is judged by them; one that does not is a candidate the message itself settles.
fn may_be_reply(event: &EventRecord, actor: &str, sender: &str) -> bool {
    if event.kind != "message.sent" || !event.subject.starts_with("message/") {
        return false;
    }
    let endpoint = |name: &str| event.body.pointer(&format!("/fields/{name}")).and_then(Value::as_str);
    endpoint("to").is_none_or(|to| to == actor) && endpoint("from").is_none_or(|from| from == sender)
}

/// Wait until `sender` writes to `actor` or the deadline passes. The graph's event feed wakes the
/// wait, but only a `message.sent` event addressed to the seat from the sender costs a read, and
/// that read is the one message it names, never the mailbox. The mailbox is listed once, and only
/// for `--after`, to find a reply that landed before the wait began. Event reads are paced.
async fn wait_for_reply(
    client: &Client,
    actor: &str,
    sender: &str,
    after: Option<u64>,
    timeout: Duration,
) -> Result<Option<MessageView>> {
    let deadline = tokio::time::Instant::now() + timeout;
    // Capture the index before looking, so a message that lands during the look is still on the
    // event feed from this cursor.
    let health: Value = client.get("/v1/health").await?;
    let started = health["store_index"]
        .as_u64()
        .context("the daemon health response has no store index")?;
    let floor = after.unwrap_or(started);
    if after.is_some()
        && let Some(message) = unread_replies(client, actor, sender, floor).await?
    {
        return Ok(Some(message));
    }
    let mut cursor = started;
    let mut event_feed = LocalEventFeed::default();
    let mut last_read: Option<tokio::time::Instant> = None;
    loop {
        let mut remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Some(last) = last_read {
            let pause = REPLY_WAIT_PACE.saturating_sub(last.elapsed()).min(remaining);
            if !pause.is_zero() {
                tokio::time::sleep(pause).await;
                remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            }
        }
        if remaining.is_zero() {
            return Ok(None);
        }
        last_read = Some(tokio::time::Instant::now());
        let wait_ms = remaining.as_millis().clamp(1, 30_000);
        let page = event_feed
            .read(client, &format!("after={cursor}&wait=true&timeout_ms={wait_ms}"))
            .await?;
        for event in &page.items {
            if event.store_index <= floor || !may_be_reply(event, actor, sender) {
                continue;
            }
            let message = read_message(client, &event.subject).await?;
            if message.from == sender
                && message.to == actor
                && message.created_index > floor
                && matches!(message.status.as_str(), "sent" | "staged" | "delivered")
            {
                return Ok(Some(message));
            }
        }
        if let Some(scanned) = page.next_after {
            cursor = cursor.max(scanned);
        }
        // A full page means more events are waiting: read the next one without a pause.
        if page.items.len() >= 200 {
            last_read = None;
        }
    }
}

fn print_reply_wait(reply: Option<&MessageView>, json_output: bool) -> Result<()> {
    match (reply, json_output) {
        (Some(message), true) => print_value(&json!({"reply": true, "message": message}), true),
        (None, true) => print_value(&json!({"reply": false}), true),
        (Some(message), false) => {
            println!("{}", message.content);
            Ok(())
        }
        (None, false) => {
            println!("no reply");
            Ok(())
        }
    }
}

fn thread_root_from_links(subject: &str, links: &BTreeMap<String, Option<String>>) -> String {
    let mut current = subject.to_owned();
    let mut seen = BTreeSet::new();
    while let Some(parent) = links.get(&current).and_then(Option::as_deref) {
        if !seen.insert(current.clone()) {
            break;
        }
        let normalized = if parent.starts_with("message/") {
            parent.to_owned()
        } else {
            format!("message/{parent}")
        };
        if !links.contains_key(&normalized) {
            break;
        }
        current = normalized;
    }
    current
}
