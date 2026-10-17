use super::*;
// The same print macros as main.rs: CLI output goes through st3's writer.
use st3::{cli_print as print, cli_println as println};

async fn run_handoff(
    client: &Client,
    configured_person: Option<&str>,
    command: HandoffCommand,
    json_output: bool,
) -> Result<()> {
    use std::io::Read as _;
    let document = |agent: &str| {
        st3::handoff::document_name(agent)
            .with_context(|| format!("`{agent}` is not an agent; only a seat has a handoff"))
    };
    match command {
        HandoffCommand::Write { file, actor } => {
            let agent = configured_actor(actor.as_deref(), configured_person, "handoff write")?;
            let name = document(&agent)?;
            let bytes = match file.as_deref() {
                Some(path) if path != Path::new("-") => {
                    fs::read(path).with_context(|| format!("read handoff {}", path.display()))?
                }
                _ => {
                    let mut bytes = Vec::new();
                    std::io::stdin().read_to_end(&mut bytes)?;
                    bytes
                }
            };
            anyhow::ensure!(
                !String::from_utf8_lossy(&bytes).trim().is_empty(),
                "a handoff cannot be empty"
            );
            let hash = hex::encode(Sha256::digest(&bytes));
            let versions: DocumentListResponse = client
                .get(&format!("/v1/documents?name={}", urlencoding::encode(&name)))
                .await?;
            let selected = versions.items.iter().find(|version| version.latest);
            let response: DocumentVersion = client
                .post(
                    "/v1/documents",
                    &DocumentPutRequest {
                        idempotency_key: format!("document:{name}:{hash}"),
                        name,
                        bytes,
                        expected_document: selected.map(|version| version.binding_claim_id.clone()),
                    },
                )
                .await?;
            if json_output {
                print_value(&response, true)
            } else {
                println!("{}@{}", response.name, response.hash);
                Ok(())
            }
        }
        HandoffCommand::Ready { actor } => {
            let agent = configured_actor(actor.as_deref(), configured_person, "handoff ready")?;
            document(&agent)?;
            let claim: ClaimRecord = client
                .post(
                    "/v1/claims",
                    &ClaimInput {
                        subject: st3::handoff::ready_subject(&agent),
                        kind: st3::handoff::READY_KIND.into(),
                        actor: Some(agent.clone()),
                        fields: std::collections::BTreeMap::from([("agent".into(), agent.clone().into())]),
                        evidence: vec![],
                        expected_subject: None,
                        idempotency_key: Some(format!("restart-ready:{agent}:{}", uuid::Uuid::now_v7())),
                    },
                )
                .await?;
            if json_output {
                print_value(&claim, true)
            } else {
                println!("{agent} is ready to restart");
                Ok(())
            }
        }
        HandoffCommand::Show { agent } => {
            let agent = configured_actor(agent.as_deref(), configured_person, "handoff show")?;
            let name = document(&agent)?;
            let versions: DocumentListResponse = client
                .get(&format!(
                    "/v1/documents?name={}&limit=1",
                    urlencoding::encode(&name)
                ))
                .await?;
            let Some(selected) = versions.items.into_iter().find(|version| version.latest) else {
                anyhow::bail!("{agent} has written no handoff");
            };
            let response: Value = client
                .get(&format!(
                    "/v1/documents/content?reference={}",
                    urlencoding::encode(&format!("{}@{}", selected.name, selected.hash))
                ))
                .await?;
            let bytes = serde_json::from_value::<Vec<u8>>(
                response
                    .get("bytes")
                    .cloned()
                    .context("document response lacks bytes")?,
            )?;
            if json_output {
                print_value(
                    &json!({
                        "agent": agent,
                        "document": selected,
                        "text": String::from_utf8_lossy(&bytes),
                    }),
                    true,
                )
            } else {
                use std::io::Write as _;
                cli_output::stdout().write_all(&bytes)?;
                Ok(())
            }
        }
    }
}

pub(super) async fn run_agents(
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    command: AgentsCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        AgentsCommand::AcknowledgeSeed(args) => {
            let actor = args.actor.as_deref().or(configured_person)
                .context("st agents acknowledge-seed needs --as ACTOR or a configured person")?;
            let claim = st3::native_seed::acknowledge(
                &cli_client(endpoint), &normalize_agent_subject(&args.subject), actor, &args.reason,
            ).await?;
            print_value(&claim, json_output)
        }
        AgentsCommand::Hold(args) => {
            let client = cli_client(endpoint);
            let subject = seat_subject(&args.subject);
            if args.duration.is_some() || args.release {
                let duration = args
                    .duration
                    .as_deref()
                    .map(|value| st3::graph::parse_duration(value, true))
                    .transpose()?;
                let now = u64::try_from(current_unix_ms()?)?;
                let claim: ClaimRecord = client
                    .post(
                        "/v1/delivery/hold",
                        &st3::delivery_hold::HoldRequest {
                            subject: subject.clone(),
                            actor: args
                                .actor
                                .or_else(|| configured_person.map(str::to_owned))
                                .context("a hold needs --as or a configured person")?,
                            held: !args.release,
                            until_unix_ms: duration
                                .map(|duration| {
                                    now.checked_add(duration).context("hold expiry overflows")
                                })
                                .transpose()?
                                .unwrap_or(0),
                            reason: args
                                .reason
                                .context("setting or releasing a hold needs --reason")?,
                            idempotency_key: format!("delivery-hold:{}", uuid::Uuid::now_v7()),
                            legacy_adoption: false,
                        },
                    )
                    .await?;
                if json_output {
                    return print_value(&claim, true);
                }
            } else {
                anyhow::ensure!(
                    args.actor.is_none() && args.reason.is_none(),
                    "--as and --reason need --for or --release"
                );
            }
            let hold: st3::delivery_hold::HoldView = client
                .get(&format!(
                    "/v1/delivery/hold?subject={}",
                    urlencoding::encode(&subject)
                ))
                .await?;
            if json_output {
                return print_value(&hold, true);
            }
            let expiry = if hold.active {
                hold.until_unix_ms
                    .and_then(|value| i64::try_from(value).ok())
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|value| {
                        format!(
                            " until {}",
                            value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                        )
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            println!(
                "{} delivery {}{}{}",
                subject,
                if hold.active { "held" } else { "enabled" },
                expiry,
                hold.reason
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            );
            Ok(())
        }
        AgentsCommand::Rename(args) => {
            let actor = args.actor.as_deref().or(configured_person).context(
                "st3 agents rename needs --as ACTOR or a configured person",
            )?;
            let response: Value = cli_client(endpoint).post(
                "/v1/agents/rename",
                &json!({
                    "subject": normalize_agent_subject(&args.subject),
                    "name": args.label,
                    "actor": actor,
                    "idempotency_key": uuid::Uuid::now_v7().to_string(),
                }),
            ).await?;
            print_value(&response, json_output)
        }
        AgentsCommand::Queue(args) => {
            run_agent_queue(endpoint, configured_person, args, json_output).await
        }
        AgentsCommand::New(args) => {
            run_agent_new(endpoint, configured_person, args, json_output).await
        }
        AgentsCommand::Repos { host } => {
            let response = generated_client(endpoint, None)?
                .host_repositories(host.as_deref().unwrap_or("local"))
                .await?;
            if json_output {
                return print_value(&response, true);
            }
            println!("REPOSITORIES  {}", response.value.host_id);
            for repository in response.value.repositories {
                println!("{}\t{}", repository.path, repository.agent_ids.join(", "));
            }
            Ok(())
        }
        AgentsCommand::Handoff { command } => {
            run_handoff(&cli_client(endpoint), configured_person, command, json_output).await
        }
        AgentsCommand::Workspace { subject } => {
            let response = generated_client(endpoint, None)?
                .agent_workspace_get(&normalize_agent_subject(&subject))
                .await?;
            if json_output {
                return print_value(&response, true);
            }
            println!("{}", response.value.workspace);
            Ok(())
        }
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
        AgentsCommand::Apply(args) => {
            eprintln!(
                "st: agents apply is legacy; use st apply FILE --no-gate-check with the same options"
            );
            let client = cli_client(endpoint);
            if !args.dry_run {
                let (kdl, source_name) = read_intent(Some(&args.file))?;
                let response = publish_text(
                    &client,
                    kdl,
                    source_name.unwrap_or_else(|| "standard input".into()),
                    args.actor,
                )
                .await?;
                return print_value(&response, json_output);
            }
            publish_mission_file(
                &client,
                MissionPublishArgs {
                    file: args.file,
                    dry_run: true,
                    at_index: None,
                    actor: args.actor,
                    workspace: PathBuf::from("."),
                    no_gate_check: true,
                    check: false,
                    inputs: vec![],
                },
                json_output,
            )
            .await
        }
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases
        AgentsCommand::Start(args) => {
            let client = cli_client(endpoint);
            let (subject, tokens, existing, mission) =
                agent_start_declaration(&client, &args).await?;
            anyhow::ensure!(!args.source_offline || existing.is_some(),
                "--source-offline needs an existing seat with a pending placement handoff");
            let response = if let Some(mission) = mission {
                anyhow::ensure!(
                    args.harness.is_none()
                        && args.host.is_none()
                        && args.workspace.is_none()
                        && args.model.is_none()
                        && args.effort.is_none()
                        && args.arguments.is_empty()
                        && !args.print_kdl,
                    "`{subject}` is a mission seat: it starts on its run's declaration; change \
                     that declaration through its mission"
                );
                if let MissionSeatStart::Declared(run) = mission {
                    if args.source_offline {
                        let agent = agent_start_status(endpoint, &client, &subject, &args).await?;
                        if json_output { return print_value(&agent, true); }
                        print!("{}", render_client_agent(&agent, &[], current_unix_ms()?));
                        return Ok(());
                    }
                    println!(
                        "{subject} is declared by {run}; `st agents restart {subject}` relaunches it"
                    );
                    return Ok(());
                }
                client
                    .post::<_, ApplyResponse>(
                        "/v1/agents/start",
                        &json!({
                            "subject": subject,
                            "actor": args.actor,
                            "expected": tokens.first(),
                            "idempotency_key": format!("st-agents-start:{}", uuid::Uuid::now_v7().simple()),
                        }),
                    )
                    .await?
            } else {
                let kdl = agent_start_document(&args, existing.as_ref())?;
                if args.print_kdl {
                    print!("{kdl}");
                    return Ok(());
                }
                publish_text_with_expected(
                    &client,
                    kdl,
                    format!("st agents start {}", args.identity),
                    args.actor.clone(),
                    Some((&subject, &tokens)),
                )
                .await?
            };
            let agent = agent_start_status(endpoint, &client, &subject, &args).await?;
            if json_output {
                let mut value = serde_json::to_value(&response)?;
                value["handoff"] = serde_json::to_value(&agent.handoff)?;
                print_value(&value, true)?;
            } else {
                print_value(&response, false)?;
                if let Some(handoff) = &agent.handoff {
                    println!("{subject}: {} → {}", handoff.phase, handoff.destination);
                    if !handoff.pending_sources.is_empty() {
                        println!(
                            "Waiting for source stop: {}",
                            handoff.pending_sources.join(", ")
                        );
                    }
                    if !handoff.overridden_sources.is_empty() {
                        println!(
                            "Source-offline override recorded: {}",
                            handoff.overridden_sources.join(", ")
                        );
                    }
                } else {
                    let state = cli_help::agent_state(
                        &agent.state,
                        agent.harness_state.as_deref(),
                        agent.fault.as_deref(),
                        &agent.reachability,
                    );
                    print!(
                        "{}",
                        cli_help::agent_next_steps(&subject, &args.actor, &state)
                    );
                }
            }
            Ok(())
        }
        AgentsCommand::Retire(args) => {
            let subject = if args.subject.starts_with("agent/") {
                args.subject.clone()
            } else {
                format!("agent/{}", args.subject)
            };
            let client = generated_client(endpoint, Some(&args.actor))?;
            let status = status_for(&cli_client(endpoint), &subject).await?;
            let token = status
                .subjects
                .first()
                .and_then(|subject| subject.desired_token.clone())
                .context("agent has no selected declaration")?;
            let snapshot = client.capabilities().await?.snapshot.id;
            let action_id = format!(
                "action/retire-{}",
                hex::encode(Sha256::digest(args.idempotency_key.as_bytes()))
            );
            let result = client
                .agent_retire(
                    action_id,
                    args.idempotency_key,
                    ClientFence {
                        snapshot_id: snapshot,
                        runtime_desired_revision: Some(token),
                        ..ClientFence::default()
                    },
                    st3_client::AgentRetireParameters {
                        agent: subject.clone(),
                        reason: args.reason,
                    },
                )
                .await?;
            if json_output {
                print_value(&result, true)
            } else {
                println!("Retired {subject}");
                Ok(())
            }
        }
        AgentsCommand::Suspend(args) => {
            let subject = format!("agent/{}", args.subject);
            let agent = request_suspension(
                endpoint,
                "/v1/agents/suspend",
                &subject,
                &args.actor,
                args.reason.as_deref(),
                None,
                &args.timeout,
            )
            .await?;
            if json_output {
                print_value(&agent, true)
            } else {
                let suspension = agent.suspension.as_ref();
                println!(
                    "Suspended {subject}: {} session {}; resume it with `st agents resume {subject}`",
                    suspension
                        .and_then(|item| item.harness.as_deref())
                        .unwrap_or("native"),
                    suspension
                        .and_then(|item| item.native_session_id.as_deref())
                        .unwrap_or("unknown"),
                );
                Ok(())
            }
        }
        AgentsCommand::Resume(args) => {
            let subject = format!("agent/{}", args.subject);
            let agent = request_suspension(
                endpoint,
                "/v1/agents/resume",
                &subject,
                &args.actor,
                None,
                args.host.as_deref(),
                &args.timeout,
            )
            .await?;
            if json_output {
                print_value(&agent, true)
            } else {
                println!(
                    "Resumed {subject} on its native session {}: running on incarnation {}",
                    agent
                        .suspension
                        .as_ref()
                        .and_then(|item| item.native_session_id.as_deref())
                        .unwrap_or("unknown"),
                    agent.incarnation_id.as_deref().unwrap_or("")
                );
                Ok(())
            }
        }
        AgentsCommand::Rollout(args) => {
            let subject = format!("agent/{}", args.subject);
            let client = cli_client(endpoint);
            let status = status_for(&client, &subject).await?;
            let current = status
                .subjects
                .iter()
                .find(|item| item.subject == subject)
                .context("no declared seat")?;
            let incarnation = current
                .actual
                .as_ref()
                .and_then(|value| value.get("incarnation_id"))
                .and_then(Value::as_str)
                .context("no recorded incarnation")?;
            let response: Value = client.post("/v1/agents/rollout",&json!({"subject":subject,"actor":args.actor,
                "expected_desired":current.desired_token,"expected_incarnation":incarnation,
                "policy":st3::rollout::Policy::when_idle(st3::graph::parse_duration(&args.deadline,true)?,args.force_after_deadline),
                "idempotency_key":uuid::Uuid::now_v7().to_string()})).await?;
            return print_value(&response, json_output);
        }
        AgentsCommand::Restart(args) => {
            let timeout = st3::graph::parse_duration(&args.timeout, false)?;
            let subject = format!("agent/{}", args.subject);
            let client = cli_client(endpoint);
            let request: ClaimRecord = client
                .post(
                    "/v1/agents/restart",
                    &json!({
                        "subject": subject,
                        "actor": args.actor,
                        "idempotency_key": uuid::Uuid::now_v7().to_string(),
                        "now": args.now,
                    }),
                )
                .await?;
            let previous = request.body["fields"]["incarnation_id"]
                .as_str()
                .unwrap_or("");
            let gateway = generated_client(endpoint, None)?;
            let wait = async {
                let mut cursor = request.store_index;
                let mut event_feed = LocalEventFeed::default();
                loop {
                    let status = status_for(&client, &subject).await?;
                    let current = status
                        .subjects
                        .iter()
                        .find(|item| item.subject == subject)
                        .context("the seat disappeared during restart")?;
                    anyhow::ensure!(
                        current.conflicts.is_empty()
                            && current.desired_token.as_deref()
                                == request.body.pointer("/evidence/0").and_then(Value::as_str),
                        "`{subject}` declaration changed during restart; inspect it with `st agents show {subject}`"
                    );
                    let response = gateway.agents_get(&subject).await?;
                    if let ClientResource::Agent(agent) = response.value {
                        if agent.state == "running"
                            && agent
                                .incarnation_id
                                .as_deref()
                                .is_some_and(|incarnation| incarnation != previous)
                        {
                            return Ok::<_, anyhow::Error>(agent);
                        }
                        if matches!(agent.state.as_str(), "failed" | "stopped")
                            && agent
                                .incarnation_id
                                .as_deref()
                                .is_some_and(|incarnation| incarnation != previous)
                        {
                            anyhow::bail!(
                                "`{subject}` replacement is {}: {}; inspect it with `st agents show {subject}`",
                                agent.state,
                                agent
                                    .fault
                                    .as_deref()
                                    .or(agent.harness_state.as_deref())
                                    .unwrap_or("the replacement exited before becoming ready")
                            );
                        }
                        // The old incarnation's fault is often why this restart was
                        // requested. It cannot fail the accepted replacement operation.
                        if let Some(fault) = agent.fault.as_deref().filter(|_| {
                            agent.incarnation_id.as_deref()
                                .is_some_and(|incarnation| incarnation != previous)
                        }) {
                            anyhow::bail!("`{subject}` could not restart: {fault}");
                        }
                        if agent.state == "waiting"
                            && agent
                                .incarnation_id
                                .as_deref()
                                .is_some_and(|incarnation| incarnation != previous)
                        {
                            anyhow::bail!(
                                "`{subject}` restarted and is waiting for your input; attach with `st terminals attach {subject}`"
                            );
                        }
                    }
                    let page = event_feed
                        .read(
                            &client,
                            &format!(
                                "after={cursor}&subject={}&wait=true&timeout_ms=1000",
                                urlencoding::encode(&subject),
                            ),
                        )
                        .await?;
                    if let Some(scanned) = page.next_after {
                        cursor = cursor.max(scanned);
                    }
                    for event in page.items {
                        let fields = event.body.get("fields").unwrap_or(&event.body);
                        if event.kind == "runtime.action.failed"
                            && fields["action"] == "restart"
                            && event.body["evidence"].as_array().is_some_and(|evidence| {
                                evidence.iter().any(|id| id.as_str() == Some(request.id.as_str()))
                            })
                        {
                            anyhow::bail!(
                                "`{subject}` could not restart: {}; inspect it with `st agents show {subject}`",
                                fields["reason"]
                                    .as_str()
                                    .unwrap_or("the runtime could not restart")
                            );
                        }
                        cursor = cursor.max(event.store_index);
                    }
                }
            };
            let agent = tokio::time::timeout(Duration::from_millis(timeout), wait).await
                .with_context(|| format!("`{subject}` did not reach a new running incarnation within {}; inspect it with `st agents show {subject}`", args.timeout))??;
            if json_output {
                print_value(&agent, true)
            } else {
                println!(
                    "Restarted {subject}: running on incarnation {}",
                    agent.incarnation_id.as_deref().unwrap_or("")
                );
                Ok(())
            }
        }
        AgentsCommand::Stop(args) => {
            let subject = resolve_member_subject(
                endpoint,
                &args.subject,
                "agent",
                Entity::Agent { running_only: true },
                completion::Matching::Exact,
            )
            .await?;
            let kdl = publication_document(kdl_node("stop", [subject.as_str()]));
            if args.print_kdl {
                print!("{kdl}");
                return Ok(());
            }
            let response = publish_text(
                &cli_client(endpoint),
                kdl,
                format!("st agents stop {subject}"),
                args.actor,
            )
            .await?;
            print_value(&response, json_output)
        }
        command => run_agent_inspection(endpoint, command, json_output).await,
    }
}

/// What `st agents start` found for a seat its mission run declared.
enum MissionSeatStart {
    /// The run still declares it; the run's mission run subject.
    Declared(String),
    /// Someone stopped it; the daemon restores the run's declaration.
    Stopped,
}

async fn agent_start_status(
    endpoint: &Endpoint,
    client: &Client,
    subject: &str,
    args: &AgentStartArgs,
) -> Result<st3_client::Agent> {
    let generated = generated_client(endpoint, None)?;
    let ClientResource::Agent(mut agent) = generated.agents_get(subject).await?.value else {
        anyhow::bail!("`{subject}` is not an agent");
    };
    if args.source_offline {
        let handoff = agent.handoff.as_ref()
            .context("--source-offline needs a placement handoff with former source hosts")?;
        if !handoff.pending_sources.is_empty() {
            client.post::<_, st3::model::ClaimRecord>("/v1/agents/source-offline", &json!({
                "subject": subject,
                "actor": args.actor,
                "desired_token": handoff.desired_token,
                "sources": handoff.pending_sources,
                "idempotency_key": format!("source-offline:{}", uuid::Uuid::now_v7()),
            })).await?;
            let ClientResource::Agent(updated) = generated.agents_get(subject).await?.value else {
                anyhow::bail!("`{subject}` is not an agent");
            };
            agent = updated;
        }
    }
    Ok(agent)
}

async fn agent_start_declaration(
    client: &Client,
    args: &AgentStartArgs,
) -> Result<(
    String,
    Vec<String>,
    Option<st3::model::DesiredSubject>,
    Option<MissionSeatStart>,
)> {
    let mut subject = format!("agent/{}", args.identity);
    let mut status = status_for(client, &subject).await?;
    if status
        .subjects
        .iter()
        .all(|item| item.desired_token.is_none())
        && !args.identity.contains(['/', '.'])
    {
        let host = if let Some(host) = args.host.as_ref().filter(|host| host.as_str() != "local") {
            host.clone()
        } else {
            let health: Value = client.get("/v1/health").await?;
            health["node"]
                .as_str()
                .context("daemon health has no node")?
                .into()
        };
        subject = format!("agent/{host}.{}", args.identity);
        status = status_for(client, &subject).await?;
    }
    let Some(current) = status.subjects.iter().find(|item| item.subject == subject) else {
        return Ok((subject, Vec::new(), None, None));
    };
    anyhow::ensure!(
        current.conflicts.is_empty(),
        "`{subject}` has conflicting declarations; resolve them before starting it"
    );
    let Some(token) = &current.desired_token else {
        return Ok((subject, Vec::new(), None, None));
    };
    let mut claim: st3::model::ClaimRecord =
        client.get(&format!("/v1/claims/by-id/{token}")).await?;
    loop {
        let desired: st3::model::DesiredSubject = serde_json::from_value(claim.body)?;
        if desired.kind == "agent" {
            // A mission seat starts on its run's own declaration, never as a new root seat.
            let mission = desired.owner_run.clone().map(|run| {
                if claim.id == *token {
                    MissionSeatStart::Declared(run)
                } else {
                    MissionSeatStart::Stopped
                }
            });
            return Ok((subject, vec![token.clone()], Some(desired), mission));
        }
        anyhow::ensure!(
            desired.kind == "stop" && claim.predecessors.len() == 1,
            "`{subject}` has no unambiguous prior agent declaration; use `st apply`"
        );
        claim = client
            .get(&format!("/v1/claims/by-id/{}", claim.predecessors[0]))
            .await?;
    }
}

async fn run_agent_new(
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    mut args: AgentNewArgs,
    json_output: bool,
) -> Result<()> {
    let client = cli_client(endpoint);
    if let Some(repository) = &args.repo {
        let health: Value = client.get("/v1/health").await?;
        let local = health["node"]
            .as_str()
            .context("the daemon health response has no node")?;
        let host = args
            .host
            .as_deref()
            .unwrap_or(local)
            .trim_start_matches("host/");
        if host == local || host == "local" {
            args.repo =
                Some(fs::canonicalize(repository).or_else(|_| std::path::absolute(repository))?);
        } else {
            anyhow::ensure!(
                repository.is_absolute(),
                "a repository on {host} must be an absolute path on that host"
            );
        }
    }
    let actor = match &args.actor {
        Some(actor) => actor.clone(),
        None => configured_human(None, configured_person, "agents new")?,
    };
    // Another host's workspace and terminal are reached as a person, like any client.
    let person = Some(actor.as_str())
        .filter(|actor| actor.starts_with("person/"))
        .or(configured_person);
    let timeout = parse_timeout(&args.timeout)?;
    let (workspace, create_workspace) =
        agent_new_workspace(&client, endpoint, &args, person).await?;
    let kdl = agent_new_document(&args, &workspace, create_workspace);
    if args.print_kdl {
        print!("{kdl}");
        return Ok(());
    }
    let health: Value = client.get("/v1/health").await?;
    let local = health["node"]
        .as_str()
        .context("the daemon health response has no node")?;
    let host = args
        .host
        .as_deref()
        .unwrap_or(local)
        .trim_start_matches("host/");
    if host == local || host == "local" {
        match client.get::<Vec<String>>("/v1/harnesses").await {
            Ok(available) => anyhow::ensure!(
                available.contains(&args.harness),
                "{} is not installed on the daemon's login PATH; install it and open a new login shell before creating a seat",
                args.harness
            ),
            // An install does not restart the daemon, so a new st can meet an older daemon.
            Err(error)
                if st3::client::is_missing_route(&error) || st3::client::is_not_found(&error) =>
            {
                eprintln!(
                    "st: the daemon is older than this st and cannot check which providers are installed; creating the seat anyway"
                );
            }
            Err(error) => return Err(error),
        }
    }
    let source_name = format!("st agents new {}", args.name);
    let preview: MissionResponse = client
        .post(
            "/v1/intent/mission",
            &MissionRequest {
                intent: IntentInput {
                    kdl: kdl.clone(),
                    source_name: Some(source_name.clone()),
                },
                at_index: None,
            },
        )
        .await?;
    anyhow::ensure!(
        preview.blockers.is_empty(),
        "{}",
        preview.blockers.join("; ")
    );
    let subject = preview
        .subject_tokens
        .keys()
        .find(|subject| subject.starts_with("agent/"))
        .cloned()
        .context("the declaration names no agent")?;
    let gateway = generated_client(endpoint, None)?;
    match gateway.agents_get(&subject).await {
        Ok(existing) => {
            if let ClientResource::Agent(agent) = existing.value
                && agent.state != "stopped"
            {
                anyhow::bail!(
                    "`{subject}` already exists and is {}; change it with `st apply`, or stop it with `st agents stop` first",
                    agent.state
                );
            }
        }
        Err(GeneratedClientError::Api(ClientErrorCode::NotFound, _, _)) => {}
        Err(error) => return Err(error.into()),
    }
    if actor.starts_with("person/") {
        let generated = generated_client(endpoint, Some(&actor))?;
        let capabilities = generated.capabilities().await?;
        let nonce = uuid::Uuid::now_v7().simple().to_string();
        generated
            .agent_create(
                format!("action/{nonce}"),
                format!("agent-new:{nonce}"),
                ClientFence {
                    snapshot_id: capabilities.snapshot.id,
                    ..ClientFence::default()
                },
                st3_client::AgentCreateParameters {
                    name: args.name.clone(),
                    harness: args.harness.clone(),
                    host: args.host.clone(),
                    model: args.model.clone(),
                    effort: args.effort.clone(),
                    workspace: Some(workspace.clone()),
                    description: args.description.clone(),
                    message: args.message.clone(),
                    repo: args.repo.as_ref().map(|path| path.display().to_string()),
                    base: args.base.clone(),
                    branch: args.branch.clone(),
                    remove_at_run_end: args.remove_at_run_end.then_some(true),
                },
            )
            .await?;
    } else {
        publish_text(&client, kdl, source_name, actor.clone()).await?;
    }
    if !json_output {
        eprintln!("Created {subject} in {workspace}; waiting for the agent to start.");
    }
    let mut latest = None;
    let waiting = tokio::time::timeout(
        timeout,
        wait_for_agent_harness(
            &client,
            &gateway,
            &subject,
            args.attach,
            json_output,
            &mut latest,
        ),
    )
    .await;
    let agent = match waiting {
        Ok(Ok(agent)) => agent,
        other => {
            if !json_output {
                let state = latest.as_ref().map_or_else(
                    || cli_help::agent_state("starting", None, None, "unknown"),
                    |agent: &st3_client::Agent| {
                        cli_help::agent_state(
                            &agent.state,
                            agent.harness_state.as_deref(),
                            agent.fault.as_deref(),
                            &agent.reachability,
                        )
                    },
                );
                println!("{}", cli_help::agent_next_steps(&subject, &actor, &state));
            }
            return match other {
                Ok(Err(error)) => Err(error),
                Err(_) => anyhow::bail!(
                    "`{subject}` was created, but was not ready after {}; see `st agents show {subject}`",
                    args.timeout
                ),
                Ok(Ok(_)) => unreachable!(),
            };
        }
    };
    if json_output {
        print_value(
            &json!({
                "subject": subject,
                "host_id": agent.host_id,
                "workspace": workspace,
                "state": agent.state,
                "harness_state": agent.harness_state,
            }),
            true,
        )?;
    } else {
        let state = cli_help::agent_state(
            &agent.state,
            agent.harness_state.as_deref(),
            agent.fault.as_deref(),
            &agent.reachability,
        );
        println!("{}", cli_help::agent_next_steps(&subject, &actor, &state));
    }
    if args.attach {
        // The daemon has just answered for the new agent, so there is no registry fallback to name.
        attach_terminal(&client, endpoint, None, person, &subject, false, None).await?;
    }
    Ok(())
}

/// The workspace for a new agent and whether its host creates it when it is missing. A named
/// workspace on another host is taken as written. With none named, the agent's host names a new
/// directory for it below its own home.
async fn agent_new_workspace(
    client: &Client,
    endpoint: &Endpoint,
    args: &AgentNewArgs,
    person: Option<&str>,
) -> Result<(String, bool)> {
    let health: Value = client.get("/v1/health").await?;
    let local = health["node"]
        .as_str()
        .context("the daemon health response has no node")?
        .to_owned();
    let host = args.host.clone().unwrap_or_else(|| local.clone());
    let host = host.trim_start_matches("host/");
    let host = if host == "local" {
        local.as_str()
    } else {
        host
    };
    if let Some(workspace) = &args.workspace {
        if host == local {
            if let Ok(existing) = fs::canonicalize(workspace) {
                return Ok((existing.display().to_string(), false));
            }
            let workspace = std::path::absolute(workspace)
                .with_context(|| format!("resolve workspace {}", workspace.display()))?;
            return Ok((workspace.display().to_string(), true));
        }
        anyhow::ensure!(
            workspace.is_absolute(),
            "a workspace on {host} must be an absolute path on that host"
        );
        return Ok((workspace.display().to_string(), true));
    }
    let path = format!(
        "/v1/hosts/{}/agent-workspace?identity={}",
        urlencoding::encode(&host),
        urlencoding::encode(&args.name)
    );
    let answer: Result<Value> = if host == local {
        client.get(&path).await
    } else {
        let Endpoint::Unix(socket) = endpoint else {
            anyhow::bail!(
                "asking {host} for a workspace needs the local Unix endpoint; pass --workspace"
            );
        };
        let person = person.with_context(|| {
            format!(
                "asking {host} for a new workspace needs `--as person/NAME` or `person = \"person/NAME\"` in the st config; or pass --workspace"
            )
        })?;
        Client::unix_as(socket.clone(), person)?
            .with_outage_wait(DAEMON_WAIT.get().copied().unwrap_or_default(), true)
            .get(&path)
            .await
    };
    let answer = answer.with_context(|| {
        format!(
            "{host} did not name a workspace for `{}`; pass --workspace",
            args.name
        )
    })?;
    let workspace = answer["workspace"]
        .as_str()
        .with_context(|| format!("{host} returned no workspace"))?;
    Ok((workspace.to_owned(), true))
}

/// Wait until the agent's current harness is ready. A harness that waits on a person, such as at
/// a login prompt, is ready enough to attach to, so `attach` accepts it.
async fn wait_for_agent_harness(
    client: &Client,
    gateway: &GeneratedClient,
    subject: &str,
    attach: bool,
    json_output: bool,
    latest: &mut Option<st3_client::Agent>,
) -> Result<st3_client::Agent> {
    let health: Value = client.get("/v1/health").await?;
    let mut cursor = health["store_index"]
        .as_u64()
        .context("the daemon health response has no store index")?;
    let mut reported = String::new();
    let mut event_feed = LocalEventFeed::default();
    loop {
        let agent = match gateway.agents_get(subject).await {
            Ok(response) => match response.value {
                ClientResource::Agent(agent) => Some(agent),
                _ => None,
            },
            Err(GeneratedClientError::Api(ClientErrorCode::NotFound, _, _)) => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(agent) = agent {
            *latest = Some(agent.clone());
            let harness = agent.harness_state.as_deref().unwrap_or("not ready");
            match agent.state.as_str() {
                "running" => return Ok(agent),
                "waiting" if attach && agent.reachability == "reachable" => return Ok(agent),
                "waiting" if agent.reachability == "reachable" => anyhow::bail!(
                    "`{subject}` started and is waiting for your input; attach with `st terminals attach {subject}`"
                ),
                "failed" => anyhow::bail!(
                    "`{subject}` failed to start: {}",
                    agent.fault.as_deref().unwrap_or(harness)
                ),
                _ => {}
            }
            let progress = cli_help::agent_state(
                &agent.state,
                agent.harness_state.as_deref(),
                agent.fault.as_deref(),
                &agent.reachability,
            );
            if !json_output && progress != reported {
                eprintln!("{subject}: {progress}");
                reported = progress;
            }
        }
        let page = event_feed
            .read(
                client,
                &format!(
                    "after={cursor}&subject={}&wait=true&timeout_ms=30000",
                    urlencoding::encode(subject),
                ),
            )
            .await?;
        if let Some(scanned) = page.next_after {
            cursor = cursor.max(scanned);
        }
    }
}

async fn run_agent_inspection(
    endpoint: &Endpoint,
    command: AgentsCommand,
    json_output: bool,
) -> Result<()> {
    let (args, tree) = match command {
        AgentsCommand::Ls(args) => (args, false),
        AgentsCommand::Tree(args) => (args, true),
        AgentsCommand::Show { subject, all } => {
            let subject = if subject.starts_with("agent/") {
                subject
            } else {
                format!("agent/{subject}")
            };
            let response = generated_client(endpoint, None)?
                .agents_get_with_history(&subject, all)
                .await
                .with_context(|| {
                    if all {
                        format!("agent `{subject}` does not exist")
                    } else {
                        format!(
                            "agent `{subject}` is not operational; use `st agents show {subject} --all` for history"
                        )
                    }
                })?;
            if json_output {
                return print_value(&response, true);
            }
            let ClientResource::Agent(agent) = response.value else {
                anyhow::bail!("`{subject}` is not an agent resource");
            };
            let client = Client::new(endpoint.clone());
            let mut current = Vec::new();
            for work in &agent.current_work_ids {
                // The card stays useful when one step cannot be read.
                if let Ok(step) = client
                    .get::<StepRunView>(&format!("/v1/work-items/{}", urlencoding::encode(work)))
                    .await
                {
                    current.push(step);
                }
            }
            print!(
                "{}",
                render_client_agent(&agent, &current, current_unix_ms()?)
            );
            return Ok(());
        }
        AgentsCommand::New(_)
        | AgentsCommand::Workspace { .. }
        | AgentsCommand::Handoff { .. }
        | AgentsCommand::Repos { .. }
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
        | AgentsCommand::Apply(_)
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases
        | AgentsCommand::Start(_)
        | AgentsCommand::Stop(_)
        | AgentsCommand::Retire(_)
        | AgentsCommand::Rollout(_)
        | AgentsCommand::Restart(_)
        | AgentsCommand::AcknowledgeSeed(_)
        | AgentsCommand::Suspend(_)
        | AgentsCommand::Resume(_)
        | AgentsCommand::Rename(_)
        | AgentsCommand::Queue(_)
        | AgentsCommand::Hold(_) => {
            unreachable!("agent mutation and queue commands return before inspection")
        }
    };
    anyhow::ensure!(
        args.limit > 0 && args.limit <= 200,
        "the agent limit must be 1 through 200"
    );
    if args.watch {
        anyhow::ensure!(!tree, "--watch applies to agents ls");
        return run_collection_watch(
            endpoint,
            None,
            "agents",
            None,
            args.status.as_deref(),
            args.limit,
            "AGENTS",
            json_output,
        )
        .await;
    }
    let generated = generated_client(endpoint, None)?;
    let response = if let Some(status) = args.status.as_deref() {
        generated
            .agents_list_for_status(status, args.cursor.as_deref(), Some(args.limit), args.all)
            .await?
    } else {
        generated
            .agents_list(args.cursor.as_deref(), Some(args.limit), args.all)
            .await?
    };
    if json_output {
        print_value(&response, true)?;
        note_partial_page(&response.value);
        return Ok(());
    }
    let mut continuation = if tree {
        "st agents tree".to_owned()
    } else {
        "st agents ls".to_owned()
    };
    if let Some(status) = args.status.as_deref() {
        continuation.push_str(&format!(" --status {status}"));
    }
    if args.enrich {
        continuation.push_str(" --enrich");
    }
    if args.all {
        continuation.push_str(" --all");
    }
    print!(
        "{}",
        render_client_agents(&response.value, tree, args.enrich, &continuation)
    );
    note_partial_page(&response.value);
    Ok(())
}

fn mission_run_subject(value: &str) -> String {
    if value.starts_with("mission-run/") {
        value.to_owned()
    } else {
        format!("mission-run/{value}")
    }
}

async fn run_agent_queue(
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    args: AgentQueueArgs,
    json_output: bool,
) -> Result<()> {
    let Some(AgentQueueCommand::Move(args)) = args.command else {
        let agent = args.agent.context("st agents queue needs an AGENT")?;
        return show_agent_queue(endpoint, &agent, json_output).await;
    };
    let actor = args.actor.as_deref().or(configured_person).context(
        "st agents queue move needs `--as person/NAME`, `--as agent/PATH`, or `person = \"person/NAME\"` in the st config",
    )?;
    let actor = parse_queue_move_actor(actor).map_err(anyhow::Error::msg)?;
    let agent = seat_subject(&args.agent);
    let (placement, anchor) = if args.top {
        (st3_client::AgentQueuePlacement::Top, None)
    } else if args.bottom {
        (st3_client::AgentQueuePlacement::Bottom, None)
    } else if let Some(before) = args.before.as_deref() {
        (
            st3_client::AgentQueuePlacement::Before,
            Some(mission_run_subject(before)),
        )
    } else if let Some(after) = args.after.as_deref() {
        (
            st3_client::AgentQueuePlacement::After,
            Some(mission_run_subject(after)),
        )
    } else {
        anyhow::bail!("choose one of --top, --bottom, --before RUN, or --after RUN");
    };
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    if actor.starts_with("agent/") {
        // Client-v0 actions carry person authority only. The daemon checks an agent's queue
        // authority on this route.
        let claim: ClaimRecord = cli_client(endpoint)
            .post(
                "/v1/agent-queue-moves",
                &st3::model::SeatQueueMoveRequest {
                    agent: agent.clone(),
                    run: mission_run_subject(&args.run),
                    placement: match placement {
                        st3_client::AgentQueuePlacement::Top => "top",
                        st3_client::AgentQueuePlacement::Bottom => "bottom",
                        st3_client::AgentQueuePlacement::Before => "before",
                        st3_client::AgentQueuePlacement::After => "after",
                    }
                    .into(),
                    anchor,
                    reason: args.reason,
                    actor,
                    idempotency_key: format!("agent-queue-move:{nonce}"),
                },
            )
            .await?;
        if json_output {
            return print_value(&claim, true);
        }
        let queue = generated_client(endpoint, None)?
            .agent_queue(&agent)
            .await?;
        print!("{}", render_agent_queue(&queue.value));
        return Ok(());
    }
    let client = generated_client(endpoint, Some(&actor))?;
    let capabilities = client.capabilities().await?;
    let response = client
        .agent_queue_move(
            format!("action/{nonce}"),
            format!("agent-queue-move:{nonce}"),
            ClientFence {
                snapshot_id: capabilities.snapshot.id,
                ..ClientFence::default()
            },
            st3_client::AgentQueueMoveParameters {
                agent_id: agent.clone(),
                mission_run_id: mission_run_subject(&args.run),
                placement,
                anchor_run_id: anchor,
                reason: args.reason,
            },
        )
        .await?;
    if json_output {
        return print_value(&response, true);
    }
    let queue = client.agent_queue(&agent).await?;
    print!("{}", render_agent_queue(&queue.value));
    Ok(())
}

/// Ask the daemon to suspend or resume `subject`, then wait for the request to finish: the seat
/// suspended, or resumed with its harness bound to the suspended native session.
async fn request_suspension(
    endpoint: &Endpoint,
    route: &str,
    subject: &str,
    actor: &str,
    reason: Option<&str>,
    host: Option<&str>,
    timeout_text: &str,
) -> Result<st3_client::Agent> {
    let timeout = st3::graph::parse_duration(timeout_text, false)?;
    let client = cli_client(endpoint);
    let request: ClaimRecord = client
        .post(
            route,
            &json!({
                "subject": subject,
                "actor": actor,
                "reason": reason,
                "host": host,
                "idempotency_key": uuid::Uuid::now_v7().to_string(),
            }),
        )
        .await?;
    let resume = route.ends_with("/resume");
    let gateway = generated_client(endpoint, None)?;
    let wait = async {
        loop {
            if let ClientResource::Agent(agent) = gateway.agents_get(subject).await?.value
                && let Some(suspension) = agent.suspension.clone()
                && suspension.operation_id == request.id
            {
                let failure = || {
                    let blocking = if suspension.blocking.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", suspension.blocking.join(", "))
                    };
                    anyhow::anyhow!(
                        "`{subject}` could not {}: {}{blocking}: {}; inspect it with `st agents show {subject}`",
                        if resume { "resume" } else { "suspend" },
                        suspension.code.as_deref().unwrap_or("failed"),
                        suspension.reason.as_deref().unwrap_or("no reason recorded"),
                    )
                };
                match (resume, suspension.phase.as_str()) {
                    (false, "suspended") | (true, "resumed") => {
                        return Ok::<_, anyhow::Error>(agent);
                    }
                    (false, "failed") => return Err(failure()),
                    (true, "suspended") if suspension.code.is_some() => return Err(failure()),
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    tokio::time::timeout(Duration::from_millis(timeout), wait)
        .await
        .with_context(|| {
            format!(
                "`{subject}` did not {} within {timeout_text}; inspect it with `st agents show {subject}`",
                if resume { "resume" } else { "suspend" }
            )
        })?
}
