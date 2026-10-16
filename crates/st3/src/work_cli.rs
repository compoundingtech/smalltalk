use super::*;
// The same print macros as main.rs: CLI output goes through st3's writer.
use st3::{cli_print as print, cli_println as println};

pub(super) async fn run_wait(client: &Client, args: WaitArgs, json_output: bool) -> Result<()> {
    validate_wait_condition(&args.condition)?;
    if let Some(actor) = args.actor.as_deref() {
        reject_foreign_agent_actor(actor)?;
    }
    let timeout = parse_timeout(&args.timeout)?;
    let actor = args.actor.as_deref().map(normalize_agent_subject);
    let wait = wait_for_condition(client, &args.subject, &args.condition, actor.as_deref());
    let value = if timeout.is_zero() {
        wait.await?
    } else {
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| anyhow::anyhow!("wait timed out after {}", args.timeout))??
    };
    print_value(&value, json_output)
}

/// Reads a structured request from a JSON file, or stdin for `-`. The daemon validates it.
fn read_structured_request(path: &Path) -> Result<serde_json::Value> {
    let text = if path == Path::new("-") {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
        text
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("read the request {}", path.display()))?
    };
    serde_json::from_str(&text).context("the request is not JSON")
}

pub(super) async fn run_work(
    client: &Client,
    endpoint: &Endpoint,
    command: WorkCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        WorkCommand::Delegation(args) => {
            reject_foreign_agent_actor(&args.actor)?;
            let result: ClaimRecord = client
                .post(
                    "/v1/work/delegation",
                    &st3::model::DelegationPolicyRequest {
                        person: args.person,
                        actor: args.actor,
                        actions: args.actions,
                        evidence: args.evidence,
                        idempotency_key: args.idempotency_key.unwrap_or_else(|| {
                            format!("delegation-policy:{}", uuid::Uuid::now_v7())
                        }),
                    },
                )
                .await?;
            print_value(&result, json_output)
        }
        WorkCommand::Start(args) => {
            reject_foreign_agent_actor(&args.actor)?;
            let response: StepRunView = client
                .post(
                    "/v1/work/start",
                    &st3::model::WorkStartRequest {
                        actor: args.actor.clone(),
                        title: args.title,
                        idempotency_key: args
                            .idempotency_key
                            .unwrap_or_else(|| format!("work-start:{}", uuid::Uuid::now_v7())),
                    },
                )
                .await?;
            if json_output {
                print_value(&response, true)
            } else {
                println!(
                    "{}\t{}\nClaim: st work claim {} --as {}",
                    response.status, response.subject, response.subject, args.actor
                );
                Ok(())
            }
        }
        WorkCommand::Handoff(args) => {
            reject_foreign_agent_actor(&args.actor)?;
            let incarnation = match args.incarnation {
                Some(value) => Some(value),
                None => current_agent_incarnation(client, &args.actor).await?,
            };
            let response: StepRunView = client
                .post(
                    &format!("/v1/work/handoff/{}", urlencoding::encode(&args.subject)),
                    &st3::model::WorkHandoffRequest {
                        actor: args.actor,
                        incarnation,
                        to: args.to,
                        note: args.note,
                        evidence: args.evidence,
                        idempotency_key: args
                            .idempotency_key
                            .unwrap_or_else(|| format!("work-handoff:{}", uuid::Uuid::now_v7())),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
        WorkCommand::Acknowledge(args) => {
            reject_foreign_agent_actor(&args.actor)?;
            let response: StepRunView = client
                .post(
                    &format!(
                        "/v1/work/acknowledge/{}",
                        urlencoding::encode(&args.subject)
                    ),
                    &st3::model::WorkAcknowledgeRequest {
                        actor: args.actor,
                        message: args.message,
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
        WorkCommand::Ask(args) => {
            reject_foreign_agent_actor(&args.actor)?;
            let mut request = args
                .request
                .as_deref()
                .map(read_structured_request)
                .transpose()?;
            if let Some(request) = request.as_mut().filter(|_| !args.attach.is_empty()) {
                let uploaded = upload_attachments(endpoint, &args.actor, &args.attach).await?;
                let mut attachments = request["attachments"].as_array().cloned().unwrap_or_default();
                attachments.extend(uploaded.iter().map(|attachment| serde_json::json!(attachment)));
                request["attachments"] = serde_json::Value::Array(attachments);
            }
            let reason = match (args.reason, &request) {
                (Some(reason), _) => reason,
                (None, Some(request)) => request["question"]
                    .as_str()
                    .context("the request needs a question; an update's text goes in --reason")?
                    .to_owned(),
                (None, None) => unreachable!("clap requires --reason or --request"),
            };
            let result: StepRunView = client
                .post(
                    "/v1/work/ask",
                    &PersonAskRequest {
                        legacy_request: None,
                        person: args.person,
                        title: args.title,
                        reason,
                        actor: args.actor,
                        step: args.step,
                        new_run: args.new_run,
                        incarnation: args.incarnation,
                        idempotency_key: args.idempotency_key,
                        request,
                    },
                )
                .await?;
            print_value(&result, json_output)
        }
        WorkCommand::Update(args) => {
            reject_foreign_agent_actor(&args.actor)?;
            let result: StepRunView = client
                .post(
                    "/v1/work/ask",
                    &PersonAskRequest {
                        legacy_request: None,
                        person: args.person,
                        title: args.title,
                        reason: args.body,
                        actor: args.actor,
                        step: None,
                        new_run: None,
                        incarnation: None,
                        idempotency_key: args.idempotency_key,
                        request: Some(
                            serde_json::json!({"version": 1, "type": "update", "about": args.about}),
                        ),
                    },
                )
                .await?;
            print_value(&result, json_output)
        }
        command @ (WorkCommand::Done(_) | WorkCommand::CancelAsk(_)) => {
            let (args, path) = match command {
                WorkCommand::Done(args) => (args, "/v1/work/done"),
                WorkCommand::CancelAsk(args) => (args, "/v1/work/cancel-ask"),
                _ => unreachable!(),
            };
            reject_foreign_agent_actor(&args.actor)?;
            let result: StepRunView = client
                .post(
                    path,
                    &PersonStepResponse {
                        delegation: args.delegation.proof(args.episode.as_deref())?,
                        subject: args.subject,
                        actor: args.actor,
                        summary: args.summary.unwrap_or_default(),
                        evidence: args.evidence,
                        episode: args.episode,
                        idempotency_key: args.idempotency_key.unwrap_or_else(|| {
                            format!("person-response:{}", uuid::Uuid::now_v7().simple())
                        }),
                        answer: (args.answer.is_some() || args.text.is_some()).then(|| {
                            st3::person_request::AnswerInput {
                                id: args.answer,
                                text: args.text,
                            }
                        }),
                    },
                )
                .await?;
            print_value(&result, json_output)
        }

        WorkCommand::Ls {
            watch,
            actor,
            all,
            cursor,
            limit,
            since,
            until,
            status,
        } => {
            if let Some(actor) = actor.as_deref() {
                reject_foreign_agent_actor(actor)?;
            }
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the work limit must be 1 through 200"
            );
            if since.is_some()
                || until.is_some()
                || status.is_some()
                || cursor
                    .as_deref()
                    .is_some_and(|c| c.starts_with("outcomes:"))
            {
                return list_outcomes(
                    client,
                    "work",
                    since,
                    until,
                    status,
                    actor,
                    cursor,
                    limit,
                    json_output,
                )
                .await;
            }
            if watch {
                return run_collection_watch(
                    endpoint,
                    None,
                    "work",
                    actor.as_deref(),
                    None,
                    limit,
                    "WORK",
                    json_output,
                )
                .await;
            }
            let generated = generated_client(endpoint, None)?;
            // A current list's first page is as of this command: it sees the command's own
            // writes, waiting for the work list to be published and retrying within the daemon
            // wait. Later pages continue the publication the first page read.
            let response = if cursor.is_none() && !all {
                generated
                    .work_list_fresh(actor.as_deref(), Some(limit))
                    .await?
            } else if let Some(actor) = actor.as_deref() {
                generated
                    .work_list_for_actor(actor, cursor.as_deref(), Some(limit), all)
                    .await?
            } else {
                generated
                    .work_list(cursor.as_deref(), Some(limit), all)
                    .await?
            };
            let mut command = "st work ls".to_owned();
            if let Some(actor) = actor.as_deref() {
                command.push_str(&format!(" --as {actor}"));
            }
            if all {
                command.push_str(" --all");
            }
            print_product_page("WORK", &response, json_output, &command)
        }
        WorkCommand::Show { subject, ignored_actor } => {
            notice_ignored_read_actor(ignored_actor.as_deref());
            let normalized = if subject.starts_with("step-run/") {
                subject
            } else {
                format!("step-run/{subject}")
            };
            let response = generated_client(endpoint, None)?
                .work_get(&normalized)
                .await?;
            if json_output {
                print_value(&response, true)
            } else {
                let ClientResource::Work(work) = &response.value else {
                    anyhow::bail!("`{normalized}` is not a work resource");
                };
                print!("{}", render_client_work_detail(work));
                Ok(())
            }
        }
        WorkCommand::Claim(args) => post_work(client, "claim", args, json_output).await,
        WorkCommand::Renew(args) => post_work(client, "renew", args, json_output).await,
        WorkCommand::Progress(args) => post_work(client, "progress", args, json_output).await,
        WorkCommand::Complete(args) => post_work(client, "complete", args, json_output).await,
        WorkCommand::Fail(args) => post_work(client, "fail", args, json_output).await,
        WorkCommand::Release(args) => post_work(client, "release", args, json_output).await,
        WorkCommand::Resume(args) => post_work(client, "resume", args, json_output).await,
        WorkCommand::Wait(args) => {
            let actor = args.actor.context("a work wait needs explicit --as")?;
            reject_foreign_agent_actor(&actor)?;
            let wait_ms = st3::graph::parse_duration(&args.wait_for, true)?;
            let subject = if args.subject.starts_with("step-run/") {
                args.subject.clone()
            } else {
                format!("step-run/{}", args.subject)
            };
            let incarnation = match args.incarnation {
                Some(incarnation) => Some(incarnation),
                None => current_agent_incarnation(client, &actor).await?,
            };
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: StepRunView = client
                .post(
                    &format!("/v1/work/wait/{}", urlencoding::encode(&subject)),
                    &WorkRequest {
                        actor: Some(actor.clone()),
                        incarnation,
                        summary: Some(format!(
                            "Waiting for {}; the execution clock is stopped for at most {}.",
                            args.reason.trim(),
                            args.wait_for
                        )),
                        reason: Some(args.reason),
                        evidence: Vec::new(),
                        wait_ms: Some(wait_ms),
                        idempotency_key: format!("work:wait:{subject}:{actor}:{nonce}"),
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
        WorkCommand::Extend(args) => {
            let actor = args.actor.context("a work extension needs explicit --as")?;
            reject_foreign_agent_actor(&actor)?;
            let by_ms = st3::graph::parse_duration(&args.by, true)?;
            let incarnation = match args.incarnation {
                Some(incarnation) => Some(incarnation),
                None => current_agent_incarnation(client, &actor).await?,
            };
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: StepRunView = client
                .post(
                    &format!("/v1/work/extend/{}", urlencoding::encode(&args.subject)),
                    &WorkExtendRequest {
                        actor: Some(actor.clone()),
                        incarnation,
                        by_ms,
                        reason: Some(args.reason),
                        idempotency_key: format!("work:extend:{}:{actor}:{nonce}", args.subject),
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
        WorkCommand::Wake(args) => {
            let actor = args
                .actor
                .context("a manual work wake needs explicit --as")?;
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: MessageView = client
                .post(
                    &format!("/v1/work/wake/{}", urlencoding::encode(&args.subject)),
                    &WorkWakeRequest {
                        actor,
                        reason: args.reason,
                        idempotency_key: format!("manual-work-wake:{}:{nonce}", args.subject),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
        WorkCommand::Retry(args) => {
            let actor = args.actor.context("a work retry needs explicit --as")?;
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: MissionRunView = client
                .post(
                    &format!("/v1/work/retry/{}", urlencoding::encode(&args.subject)),
                    &WorkRetryRequest {
                        actor,
                        reason: args.reason,
                        idempotency_key: format!("manual-work-retry:{}:{nonce}", args.subject),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
        WorkCommand::PublishMission(args) => publish_work_mission(client, args, json_output).await,
        WorkCommand::Revise(args) => {
            let actor = args
                .actor
                .context("a mission revision needs explicit --as")?;
            let kdl = fs::read_to_string(&args.file)
                .with_context(|| format!("read KDL {}", args.file.display()))?;
            let run: MissionRunView = client
                .get(&format!(
                    "/v1/mission-runs/{}",
                    urlencoding::encode(&args.run)
                ))
                .await?;
            let parsed = st3::parse_intent(&kdl, "local")?;
            let mission_id = run.mission.strip_prefix("mission/").unwrap_or(&run.mission);
            let candidate = parsed.missions.get(mission_id).with_context(|| {
                format!(
                    "{} must contain the current mission `{mission_id}`",
                    args.file.display()
                )
            })?;
            anyhow::ensure!(
                st3::mission::top_level_mission_ids(&parsed.missions).len() == 1,
                "a mission revision file must contain exactly one top-level mission"
            );
            let operation = format!("revision-{}", uuid::Uuid::now_v7().simple());
            let revision_kdl = mission_revision_intent(
                &run.subject,
                &operation,
                mission_id,
                &candidate.revision,
                &run.generation,
                &args.reason,
            );
            if args.print_kdl {
                eprintln!(
                    "Submit the candidate with `st work revise` after reviewing {} as {}",
                    args.file.display(),
                    actor
                );
                print!("{revision_kdl}");
                return Ok(());
            }
            let response: RevisionSubmissionView = client
                .post(
                    &format!(
                        "/v1/mission-runs/{}/revision",
                        urlencoding::encode(&run.subject)
                    ),
                    &MissionRevisionRequest {
                        intent: IntentInput {
                            kdl,
                            source_name: Some(args.file.display().to_string()),
                        },
                        actor,
                        reason: args.reason,
                        idempotency_key: operation,
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
        WorkCommand::Revision { command } => run_work_revision(client, command, json_output).await,
    }
}

async fn run_work_revision(
    client: &Client,
    command: WorkRevisionCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        WorkRevisionCommand::Show { run } => {
            let proposal: RevisionProposalView = client
                .get(&format!(
                    "/v1/mission-runs/{}/revision-proposal",
                    urlencoding::encode(&run)
                ))
                .await?;
            if json_output {
                print_value(&proposal, true)
            } else {
                print!(
                    "{}",
                    render_revision_proposal(&proposal, OutputStyle::stdout(), current_unix_ms()?)
                );
                Ok(())
            }
        }
        WorkRevisionCommand::Generations { run } => {
            let generations: Vec<RunGenerationView> = client
                .get(&format!(
                    "/v1/mission-runs/{}/generations",
                    urlencoding::encode(&run)
                ))
                .await?;
            if json_output {
                print_value(&generations, true)
            } else {
                let mission_run: MissionRunView = client
                    .get(&format!("/v1/mission-runs/{}", urlencoding::encode(&run)))
                    .await?;
                print!(
                    "{}",
                    render_generations(&mission_run, &generations, OutputStyle::stdout())
                );
                Ok(())
            }
        }
        WorkRevisionCommand::Generation { generation } => {
            let generation: RunGenerationView = client
                .get(&format!(
                    "/v1/run-generations/{}",
                    urlencoding::encode(&generation)
                ))
                .await?;
            if json_output {
                print_value(&generation, true)
            } else {
                print!("{}", render_generation(&generation, OutputStyle::stdout()));
                Ok(())
            }
        }
        WorkRevisionCommand::Approve {
            proposal,
            preview_hash,
            actor,
        } => {
            let actor = actor.context("a revision approval needs explicit --as")?;
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: RevisionSubmissionView = client
                .post(
                    &format!(
                        "/v1/revision-proposals/{}/approve",
                        urlencoding::encode(&proposal)
                    ),
                    &RevisionApprovalRequest {
                        actor,
                        preview_hash,
                        idempotency_key: format!("revision-approve:{proposal}:{nonce}"),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
        WorkRevisionCommand::Cancel {
            proposal,
            actor,
            reason,
        } => {
            let actor = actor.context("a revision cancellation needs explicit --as")?;
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: RevisionProposalView = client
                .post(
                    &format!(
                        "/v1/revision-proposals/{}/cancel",
                        urlencoding::encode(&proposal)
                    ),
                    &RevisionCancelRequest {
                        actor,
                        reason,
                        idempotency_key: format!("revision-cancel:{proposal}:{nonce}"),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
    }
}

async fn publish_work_mission(
    client: &Client,
    args: WorkPublishMissionArgs,
    json_output: bool,
) -> Result<()> {
    let actor = args
        .actor
        .context("publishing a mission output needs explicit --as")?;
    let incarnation = match args.incarnation {
        Some(incarnation) => Some(incarnation),
        None => current_agent_incarnation(client, &actor).await?,
    };
    let kdl = fs::read_to_string(&args.file)
        .with_context(|| format!("read KDL {}", args.file.display()))?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let output: MissionOutputView = client
        .post(
            &format!("/v1/work/mission/{}", urlencoding::encode(&args.subject)),
            &MissionProductionRequest {
                intent: IntentInput {
                    kdl,
                    source_name: Some(args.file.display().to_string()),
                },
                actor,
                incarnation,
                idempotency_key: format!("mission-output:{}:{nonce}", args.subject),
            },
        )
        .await?;
    if json_output {
        print_value(&output, true)
    } else {
        println!("{}@{}", output.mission, output.revision);
        Ok(())
    }
}

async fn post_work(
    client: &Client,
    action: &str,
    args: WorkActionArgs,
    json_output: bool,
) -> Result<()> {
    let actor = args.actor.context("a work action needs explicit --as")?;
    reject_foreign_agent_actor(&actor)?;
    let subject = if args.subject.starts_with("step-run/") { args.subject.clone() }
        else { format!("step-run/{}", args.subject) };
    let started_at_unix_ms = st_drivers::message::now_ms();
    let response: Result<StepRunView> = async {
        let incarnation = match args.incarnation {
            Some(incarnation) => Some(incarnation),
            None => current_agent_incarnation(client, &actor).await?,
        };
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        client
            .post(
                &format!("/v1/work/{action}/{}", urlencoding::encode(&args.subject)),
                &WorkRequest {
                    wait_ms: None,
                    actor: Some(actor.clone()),
                    incarnation,
                    summary: args.summary,
                    reason: args.reason,
                    evidence: args.evidence,
                    idempotency_key: format!("work:{action}:{}:{actor}:{nonce}", args.subject),
                },
            )
            .await
    }.await;
    let response = response.inspect_err(|error| {
        if action == "claim" && let Some(outage) = st3::client::daemon_unreachable(error)
            && let Some(os_error) = outage.socket_os_error()
        {
            let receipt = st_drivers::work_claim_failure::Receipt {
                actor: actor.clone(), step_run: subject.clone(),
                socket: outage.endpoint().into(), os_error, started_at_unix_ms,
                observed_at_unix_ms: st_drivers::message::now_ms(),
            };
            if receipt.valid() {
                eprintln!("{}{}", st_drivers::work_claim_failure::PREFIX,
                    serde_json::to_string(&receipt).expect("socket receipt serializes"));
            }
        }
    })?;
    if json_output {
        print_value(&response, true)
    } else {
        if action == "claim" {
            print!(
                "{}",
                render_step_run(&response, OutputStyle::stdout(), current_unix_ms()?)
            );
            if let Some((node, documents)) = host_facts(client).await {
                print!(
                    "{}",
                    render_host_facts(&node, &documents, OutputStyle::stdout())
                );
            }
        } else {
            println!("{}\t{}", response.status, response.subject);
        }
        Ok(())
    }
}

/// This machine's host documents, which a claim prints because they describe where the claimed
/// work runs. A lookup that fails prints nothing: the claim itself has already succeeded.
async fn host_facts(client: &Client) -> Option<(String, Vec<(String, String)>)> {
    let health: Value = client.get("/v1/health").await.ok()?;
    let node = health.get("node")?.as_str()?.to_owned();
    let status: StatusResponse = client
        .get(&format!(
            "/v1/status?subject={}",
            urlencoding::encode(&format!("host/{node}"))
        ))
        .await
        .ok()?;
    let desired = status.subjects.first()?.desired.clone()?;
    let mut documents = Vec::new();
    for reference in desired
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|child| child.get("name").and_then(Value::as_str) == Some("document"))
        .filter_map(|child| child.pointer("/arguments/0").and_then(Value::as_str))
    {
        let (name, hash) = reference.rsplit_once('@')?;
        let bytes = document_bytes(client, name, hash).await.ok()?;
        documents.push((reference.to_owned(), String::from_utf8(bytes).ok()?));
    }
    Some((node, documents))
}

fn mission_revision_intent(
    run: &str,
    operation_id: &str,
    mission_id: &str,
    revision: &str,
    from_generation: &str,
    reason: &str,
) -> String {
    format!(
        "version 2\nmission-run {run:?} {{\n  revision {operation_id:?} {{\n    mission {:?}\n    from {from_generation:?}\n    reason {reason:?}\n  }}\n}}\n",
        format!("mission/{mission_id}@{revision}")
    )
}
