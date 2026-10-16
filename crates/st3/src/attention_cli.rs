use super::*;
// The same print macros as main.rs: CLI output goes through st3's writer.
use st3::{cli_print as print, cli_println as println};

async fn run_review_decision(
    client: &Client,
    decision: &str,
    args: ReviewArgs,
    json_output: bool,
) -> Result<()> {
    let path = format!("/v1/reviews/{}", args.target);
    let response: ClaimRecord = client
        .post(
            &path,
            &ReviewRequest {
                delegation: args.delegation.proof(args.episode.as_deref())?,
                decision: decision.to_owned(),
                reason: args.reason,
                actor: Some(args.actor),
                expected_subject: None,
            },
        )
        .await?;
    print_value(&response, json_output)
}

/// The attention item a person can act on now, found from an `attention/...` ID or its source, and
/// the card ID when the subject was one. It must not read an update for the person.
async fn actionable_attention_item(
    client: &Client,
    endpoint: &Endpoint,
    subject: &str,
    actor: &str,
) -> Result<(AttentionItemView, Option<String>)> {
    let normalized = resolve_member_subject(
        endpoint,
        subject,
        "attention",
        Entity::Attention,
        completion::Matching::Fuzzy,
    )
    .await?;
    // Public card IDs name a recipient and waiting episode, not just a work source.
    // Resolve through the actor-scoped read projection; this must not read an update
    // for the person or let a spent card open a later episode of the same source.
    let alias = if normalized.starts_with("attention/") {
        match generated_client(endpoint, Some(actor))?
            .attention_get(&normalized)
            .await
        {
            Ok(response) => match response.value {
                ClientResource::Attention(card)
                    if card.header.id == normalized && card.person_id == actor =>
                {
                    Some(card)
                }
                _ => None,
            },
            Err(GeneratedClientError::Api(ClientErrorCode::NotFound, _, _)) => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    let path = format!("/v1/attention?person={}", urlencoding::encode(actor));
    let item = client
        .get::<Vec<AttentionItemView>>(&path)
        .await?
        .into_iter()
        .find(|item| {
            alias.as_ref().map_or_else(
                || item.subject == normalized,
                |card| item.subject == card.source_id && item.episode == card.episode,
            )
        })
        .with_context(|| {
            format!("attention item `{normalized}` is not currently actionable")
        })?;
    let card_id = alias.map(|card| card.header.id);
    Ok((item, card_id))
}

pub(super) async fn run_attention(
    client: &Client,
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    command: AttentionCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        AttentionCommand::Ls {
            watch,
            actor,
            all,
            cursor,
            limit,
        } => {
            let actor = configured_human(actor.as_deref(), configured_person, "alerts")?;
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the alerts limit must be 1 through 200"
            );
            if watch {
                return run_collection_watch(
                    endpoint,
                    Some(&actor),
                    "attention",
                    None,
                    None,
                    limit,
                    &format!("ALERTS FOR {actor}"),
                    json_output,
                )
                .await;
            }
            let response = generated_client(endpoint, Some(&actor))?
                .attention_list(cursor.as_deref(), Some(limit), all)
                .await?;
            let history = if all { " --all" } else { "" };
            let command = format!("st alerts ls --as {actor}{history}");
            if json_output {
                print_value(&response, true)?;
            } else {
                if let Some(sync) = &response.value.sync {
                    print!("{}", render_sync_notice(sync, now_ms()));
                }
                print!(
                    "{}",
                    render_alert_sections(
                        &response.value,
                        Some(&format!("ALERTS FOR {actor}")),
                        &command
                    )
                );
            }
            note_partial_page(&response.value);
            Ok(())
        }
        AttentionCommand::Show { subject, actor } => {
            let actor = configured_human(actor.as_deref(), configured_person, "alerts")?;
            let (item, _) = actionable_attention_item(client, endpoint, &subject, &actor).await?;
            if json_output {
                print_value(&item, true)?;
            } else {
                print!(
                    "{}",
                    render_attention_show(&item, OutputStyle::stdout(), now_ms())
                );
            }
            // The person opening an update reads it, which clears it from their home. An agent
            // seat looking at the person's home reads nothing for them.
            if item.request.as_ref().is_some_and(|r| r["type"] == "update")
                && item.person == actor
                && std::env::var_os("ST_AGENT").is_none()
            {
                let _: StepRunView = client
                    .post(
                        "/v1/work/done",
                        &PersonStepResponse {
                            delegation: None,
                            subject: item.subject.clone(),
                            actor: actor.clone(),
                            summary: String::new(),
                            evidence: Vec::new(),
                            episode: Some(item.episode.clone()),
                            idempotency_key: format!("update-read:{}", item.episode),
                            answer: None,
                        },
                    )
                    .await?;
                if !json_output {
                    println!("\nRead: this update has left your home.");
                }
            }
            Ok(())
        }
        AttentionCommand::Discuss(args) => {
            let actor = configured_human(args.actor.as_deref(), configured_person, "alerts")?;
            let (item, card_id) =
                actionable_attention_item(client, endpoint, &args.subject, &actor).await?;
            // The agent whose conversation the alert belongs to; an older daemon names only the
            // agent that asked.
            let to = match args.to {
                Some(to) => to,
                None => item
                    .conversation
                    .clone()
                    .or_else(|| item.requester_id.clone())
                    .filter(|agent| agent.starts_with("agent/"))
                    .context("this alert names no agent to talk to; pass --to AGENT")?,
            };
            let id = card_id.unwrap_or_else(|| item.subject.clone());
            // The same words stui's "Chat about this" sends.
            let mut body = format!("{}\n\n---\nThis is about {} ({id}", args.body, item.title);
            if let Some(mission) = &item.mission {
                body.push_str(&format!(", mission {mission}"));
            }
            body.push(')');
            let Some(receipt) = send_message(
                client,
                MessageSendArgs {
                    to,
                    body,
                    subject: Some(format!("About: {}", item.title)),
                    in_reply_to: None,
                    tags: Vec::new(),
                    from: actor,
                    attach: Vec::new(),
                    wake: MessageWakeArgs::default(),
                    print_kdl: false,
                    idempotency_key: args.idempotency_key,
                },
                Vec::new(),
            )
            .await?
            else {
                return Ok(());
            };
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
        AttentionCommand::Request(args) => {
            let actor = args
                .actor
                .context("an attention request needs explicit --as")?;
            let idempotency_key = args
                .idempotency_key
                .unwrap_or_else(|| format!("attention-request:{}", uuid::Uuid::now_v7().simple()));
            let response: AttentionRequestView = client
                .post(
                    "/v1/attention",
                    &st3::model::AttentionRequestPost {
                        request: AttentionRequest {
                            reviewer: args.reviewer,
                            title: args.title,
                            reason: args.reason,
                            severity: args.severity,
                            targets: args.targets,
                            actor,
                            idempotency_key,
                        },
                        closing: st3::model::AttentionClosing {
                            until: args.until,
                            step: args.step,
                            closed_by: args.person_closes.then(|| "person".into()),
                        },
                    },
                )
                .await?;
            if json_output {
                print_value(&response, true)
            } else {
                println!("{}\t{}", response.status, response.subject);
                if let Some(step) = &response.step {
                    eprintln!("It closes on its own when `{step}` ends.");
                }
                Ok(())
            }
        }
        AttentionCommand::Resolve(args) => {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: AttentionRequestView = client
                .post(
                    &format!(
                        "/v1/attention/resolve/{}",
                        urlencoding::encode(&args.subject)
                    ),
                    &AttentionResolveRequest {
                        delegation: args.delegation.proof(args.episode.as_deref())?,
                        outcome: args.outcome,
                        reason: args.reason,
                        actor: args.actor,
                        idempotency_key: format!("attention-resolve:{}:{nonce}", args.subject),
                    },
                )
                .await?;
            if json_output {
                print_value(&response, true)
            } else {
                println!("{}\t{}", response.status, response.subject);
                Ok(())
            }
        }
        AttentionCommand::Withdraw(args) => {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: AttentionRequestView = client
                .post(
                    &format!(
                        "/v1/attention/withdraw/{}",
                        urlencoding::encode(&args.subject)
                    ),
                    &AttentionWithdrawRequest {
                        reason: args.reason,
                        actor: args.actor,
                        idempotency_key: format!("attention-withdraw:{}:{nonce}", args.subject),
                    },
                )
                .await?;
            if json_output {
                print_value(&response, true)
            } else {
                println!("{}\t{}", response.status, response.subject);
                Ok(())
            }
        }
        AttentionCommand::Approve(args) => {
            run_review_decision(client, "approved", args, json_output).await
        }
        AttentionCommand::Reject(args) => {
            run_review_decision(client, "rejected", args, json_output).await
        }
        AttentionCommand::RequestChanges(args) => {
            run_review_decision(
                client,
                "changes-requested",
                ReviewArgs {
                    delegation: DelegationArgs::default(),
                    episode: None,
                    target: args.target,
                    reason: Some(args.reason),
                    actor: args.actor,
                },
                json_output,
            )
            .await
        }
    }
}
