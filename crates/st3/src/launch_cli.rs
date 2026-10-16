use super::*;
// The same print macros as main.rs: CLI output goes through st3's writer.
use st3::{cli_print as print, cli_println as println};

pub(super) async fn run_launch(
    client: &Client,
    endpoint: &Endpoint,
    command: LaunchCommand,
    default_planner: &PlannerSpec,
    json_output: bool,
) -> Result<()> {
    if let LaunchCommand::Ls { all, cursor, limit } = &command {
        anyhow::ensure!(
            *limit > 0 && *limit <= 200,
            "the launch limit must be 1 through 200"
        );
        let response = generated_client(endpoint, None)?
            .launches_list(cursor.as_deref(), Some(*limit), *all)
            .await?;
        let history = if *all { " --all" } else { "" };
        return print_product_page(
            "LAUNCHES",
            &response,
            json_output,
            &format!("st launch ls{history}"),
        );
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let starting = matches!(&command, LaunchCommand::Start(_));
    let response = match command {
        LaunchCommand::Ls { .. } => unreachable!(),
        LaunchCommand::Start(args) => {
            let provider = args
                .provider
                .as_deref()
                .unwrap_or(&default_planner.provider)
                .to_owned();
            let inherit_default = provider == default_planner.provider;
            let planner = PlannerSpec {
                provider,
                model: args.model.or_else(|| {
                    inherit_default
                        .then(|| default_planner.model.clone())
                        .flatten()
                }),
                effort: args.effort.or_else(|| {
                    inherit_default
                        .then(|| default_planner.effort.clone())
                        .flatten()
                }),
            };
            let (request, _) = read_intent(args.request.as_deref())?;
            anyhow::ensure!(
                !request.trim().is_empty(),
                "a launch request cannot be empty"
            );
            let workspace = fs::canonicalize(&args.workspace)
                .with_context(|| format!("resolve workspace {}", args.workspace.display()))?;
            let target = args.run.as_deref().map(|run| async {
                client
                    .get::<MissionRunView>(&format!(
                        "/v1/mission-runs/{}",
                        urlencoding::encode(run)
                    ))
                    .await
            });
            let target = match target {
                Some(target) => Some(target.await?),
                None => None,
            };
            let mission_id = target
                .as_ref()
                .map(|run| run.mission.trim_start_matches("mission/").to_owned())
                .or(args.id)
                .context("launch start needs --id or --run")?;
            let session_id = format!("launch/{mission_id}/{}", uuid::Uuid::now_v7().simple());
            let request_hash = hex::encode(Sha256::digest(request.as_bytes()));
            let request_name = format!("doc/planning/{session_id}/request");
            let request_reference = format!("{request_name}@{request_hash}");
            let requester = args.requester;
            let kdl = planning_session_intent(
                &session_id,
                &mission_id,
                &request_reference,
                &workspace,
                &requester,
                &planner,
                target.as_ref(),
            );
            if args.print_kdl {
                eprintln!(
                    "Store the request first: st documents put {} --as {}",
                    args.request
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "REQUEST_FILE".into()),
                    request_name
                );
                print!("{kdl}");
                return Ok(());
            }
            put_document_bytes(client, request_name, request.into_bytes()).await?;
            publish_text(
                client,
                kdl,
                format!("st launch start {session_id}"),
                requester.clone(),
            )
            .await?;
            client
                .get::<PlanningSessionView>(&format!(
                    "/v1/launches/{}",
                    urlencoding::encode(&session_id)
                ))
                .await?
        }
        LaunchCommand::Show(args) => {
            client
                .get::<PlanningSessionView>(&format!(
                    "/v1/launches/{}",
                    urlencoding::encode(&args.session)
                ))
                .await?
        }
        LaunchCommand::Preview(args) => {
            let path = args.variant.as_deref().map_or_else(
                || {
                    format!(
                        "/v1/launches/{}/preview",
                        urlencoding::encode(&args.session)
                    )
                },
                |variant| {
                    format!(
                        "/v1/launches/{}/variants/{}/preview",
                        urlencoding::encode(&args.session),
                        urlencoding::encode(variant)
                    )
                },
            );
            client
                .post::<_, PlanningSessionView>(&path, &json!({}))
                .await?
        }
        LaunchCommand::Submit(args) => {
            client
                .post::<_, PlanningSessionView>(
                    &format!(
                        "/v1/launches/{}/variants/{}/submit",
                        urlencoding::encode(&args.session),
                        urlencoding::encode(&args.variant)
                    ),
                    &PlanningCandidateSubmitRequest {
                        actor: args.actor,
                        markdown: fs::read(&args.markdown).with_context(|| {
                            format!("read Markdown {}", args.markdown.display())
                        })?,
                        kdl: fs::read(&args.kdl)
                            .with_context(|| format!("read KDL {}", args.kdl.display()))?,
                        idempotency_key: format!("planning-submit:{nonce}"),
                    },
                )
                .await?
        }
        LaunchCommand::Revise(args) => {
            let actor = args.actor;
            let feedback = fs::read(&args.feedback)
                .with_context(|| format!("read feedback {}", args.feedback.display()))?;
            std::str::from_utf8(&feedback).context("launch feedback must be UTF-8 text")?;
            let hash = hex::encode(Sha256::digest(&feedback));
            let session = args
                .session
                .strip_prefix("planning-session/")
                .unwrap_or(&args.session);
            let document_name = format!("doc/planning/{session}/feedback/{hash}");
            let reference = format!("{document_name}@{hash}");
            let operation = format!("feedback-{}", uuid::Uuid::now_v7().simple());
            let kdl = planning_feedback_intent(session, &operation, &reference, "default");
            if args.print_kdl {
                eprintln!(
                    "Store the feedback first: st documents put {} --as {}",
                    args.feedback.display(),
                    document_name
                );
                print!("{kdl}");
                return Ok(());
            }
            put_document_bytes(client, document_name, feedback).await?;
            publish_text(client, kdl, format!("st launch revise {session}"), actor).await?;
            client
                .get::<PlanningSessionView>(&format!(
                    "/v1/launches/{}",
                    urlencoding::encode(session)
                ))
                .await?
        }
        LaunchCommand::Approve(args) => {
            client
                .post::<_, PlanningSessionView>(
                    &format!(
                        "/v1/launches/{}/approve",
                        urlencoding::encode(&args.session)
                    ),
                    &PlanningApprovalRequest {
                        actor: args.actor,
                        preview_hash: args.preview_hash,
                        idempotency_key: format!("planning-approve:{nonce}"),
                    },
                )
                .await?
        }
        LaunchCommand::ApproveAndLaunch(args) => {
            let workspace = fs::canonicalize(&args.workspace)
                .with_context(|| format!("resolve workspace {}", args.workspace.display()))?;
            let response: LaunchApproveAndStartView = client
                .post(
                    &format!(
                        "/v1/launches/{}/approve-and-launch",
                        urlencoding::encode(&args.session)
                    ),
                    &LaunchApproveAndStartRequest {
                        actor: args.actor,
                        preview_hash: args.preview_hash,
                        workspace: workspace.to_string_lossy().into_owned(),
                        inputs: args.inputs.into_iter().collect(),
                        idempotency_key: format!("launch-approve-and-start:{nonce}"),
                    },
                )
                .await?;
            print_value(&response, json_output)?;
            if !json_output {
                print!("{}", cli_help::mission_next_steps(&response.mission_run));
            }
            return Ok(());
        }
        LaunchCommand::Run(args) => {
            let workspace = fs::canonicalize(&args.workspace)
                .with_context(|| format!("resolve workspace {}", args.workspace.display()))?;
            let response: MissionRunView = client
                .post(
                    &format!("/v1/launches/{}/start", urlencoding::encode(&args.session)),
                    &LaunchStartRequest {
                        actor: args.actor,
                        workspace: workspace.to_string_lossy().into_owned(),
                        inputs: args.inputs.into_iter().collect(),
                        idempotency_key: format!("launch-start:{nonce}"),
                    },
                )
                .await?;
            print_value(&response, json_output)?;
            if !json_output {
                print!("{}", cli_help::mission_next_steps(&response));
            }
            return Ok(());
        }
        LaunchCommand::Question(args) => {
            let response: Value = client
                .post(
                    &format!(
                        "/v1/launches/{}/decisions",
                        urlencoding::encode(&args.session)
                    ),
                    &LaunchDecisionRequest {
                        actor: args.actor,
                        question: args.question,
                        decision_type: args.decision_type,
                        options: args.options,
                        idempotency_key: format!("launch-question:{nonce}"),
                    },
                )
                .await?;
            return print_value(&response, json_output);
        }
        LaunchCommand::Answer(args) => {
            let response: Value = client
                .post(
                    &format!(
                        "/v1/launches/{}/decisions/{}/answer",
                        urlencoding::encode(&args.session),
                        urlencoding::encode(&args.decision),
                    ),
                    &LaunchDecisionAnswerRequest {
                        actor: args.actor,
                        response: args.response,
                        explanation: args.explanation,
                        expected_revision: args.expected_revision,
                        idempotency_key: format!("launch-answer:{nonce}"),
                    },
                )
                .await?;
            return print_value(&response, json_output);
        }
        LaunchCommand::Cancel(args) => {
            let actor = args.actor;
            let session = args
                .session
                .strip_prefix("planning-session/")
                .unwrap_or(&args.session);
            let operation = format!("cancel-{}", uuid::Uuid::now_v7().simple());
            let kdl = planning_cancellation_intent(
                session,
                &operation,
                args.reason.as_deref().unwrap_or("the launch was cancelled"),
            );
            if args.print_kdl {
                print!("{kdl}");
                return Ok(());
            }
            publish_text(client, kdl, format!("st launch cancel {session}"), actor).await?;
            client
                .get::<PlanningSessionView>(&format!(
                    "/v1/launches/{}",
                    urlencoding::encode(session)
                ))
                .await?
        }
        LaunchCommand::Compare(args) => {
            let response: Value = client
                .get(&format!(
                    "/v1/launches/{}/variants/{}/compare/{}",
                    urlencoding::encode(&args.session),
                    urlencoding::encode(&args.left),
                    urlencoding::encode(&args.right)
                ))
                .await?;
            return print_value(&response, json_output);
        }
        LaunchCommand::Propose(args) => {
            let actor = args
                .actor
                .context("a planning proposal needs explicit --as")?;
            let response: RevisionSubmissionView = client
                .post(
                    &format!(
                        "/v1/launches/{}/variants/{}/propose",
                        urlencoding::encode(&args.session),
                        urlencoding::encode(&args.variant)
                    ),
                    &PlanningProposalRequest {
                        actor,
                        reason: args.reason,
                        idempotency_key: format!("planning-propose:{nonce}"),
                    },
                )
                .await?;
            return print_value(&response, json_output);
        }
    };
    if json_output {
        return print_value(&response, true);
    }
    println!("{}\t{}", response.status, response.subject);
    println!("Mission: mission/{}", response.mission);
    println!("Planner: {}", response.planner);
    if let Some(candidate) = &response.candidate {
        println!(
            "Candidate: {} ({})",
            candidate.revision, candidate.mission_revision
        );
    }
    if let Some(preview) = &response.preview {
        println!("Preview: {}", preview.hash);
        println!("\nGraph:\n{}", preview.graph);
        println!("\nDiff:\n{}", preview.diff);
        for warning in &preview.mission.warnings {
            println!("Warning: {warning}");
        }
        for blocker in &preview.mission.blockers {
            println!("Blocker: {blocker}");
        }
    }
    if starting {
        print!("{}", cli_help::launch_next_steps(&response));
    }
    Ok(())
}

pub(super) async fn run_mission_view(
    client: &Client,
    endpoint: &Endpoint,
    command: MissionViewCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        MissionViewCommand::Tree => {
            let view: Value = client.get("/v1/client/missions-tree").await?;
            if json_output {
                print_value(&view, true)
            } else {
                print!("{}", render_missions_tree(&view));
                Ok(())
            }
        }
        MissionViewCommand::Ls {
            watch,
            all,
            cursor,
            limit,
            since,
            until,
            status,
        } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the mission limit must be 1 through 200"
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
                    "missions",
                    since,
                    until,
                    status,
                    None,
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
                    "missions",
                    None,
                    None,
                    limit,
                    "MISSIONS",
                    json_output,
                )
                .await;
            }
            let response = generated_client(endpoint, None)?
                .missions_list(cursor.as_deref(), Some(limit), all)
                .await?;
            let history = if all { " --all" } else { "" };
            print_product_page(
                "MISSIONS",
                &response,
                json_output,
                &format!("st missions ls{history}"),
            )
        }
        MissionViewCommand::Show(args) => {
            notice_ignored_read_actor(args.ignored_actor.as_deref());
            anyhow::ensure!((1..=50).contains(&args.limit), "mission tree limit must be 1 through 50");
            anyhow::ensure!(
                !json_output || args.cursor.is_none(),
                "--cursor continues the human tree; omit --json"
            );
            let client = if args.follow {
                client.clone().with_follow_retry()
            } else {
                client.clone()
            };
            let client = &client;
            let mut selected = args.mission_or_run;
            if !selected.starts_with("mission-run/") {
                let overview: Value = client
                    .get(&format!(
                        "/v1/mission-overview?mission={}",
                        urlencoding::encode(&selected)
                    ))
                    .await?;
                if overview["total_runs"].as_u64() != Some(1) {
                    anyhow::ensure!(
                        !args.follow,
                        "--follow needs an exact mission run subject; choose a run from the summary"
                    );
                    if json_output {
                        return print_value(&overview, true);
                    }
                    print!("{}", render_mission_overview(&overview));
                    return Ok(());
                }
                selected = overview["newest"][0]["id"]
                    .as_str()
                    .context("missing run in mission summary")?
                    .to_owned();
            }
            let run = client
                .get::<MissionRunView>(&format!(
                    "/v1/mission-runs/{}",
                    urlencoding::encode(&selected)
                ))
                .await?;
            if args.follow {
                return follow_mission_run(client, run, 0, json_output, args.limit).await;
            }
            if json_output {
                return print_value(&run, true);
            }
            let page = load_mission_run_tree(client, &run, args.cursor.as_deref(), args.limit).await?;
            let now = current_unix_ms()?;
            print!(
                "{}",
                render_mission_run_page(&run, &page.runs, OutputStyle::stdout(), now)
            );
            print_mission_tree_continuation(&run, &page, args.limit);
            // A daemon without lanes answers 404; the run itself is still shown.
            if let Ok(lanes) = client
                .get::<Vec<st3::model::LaneView>>(&format!(
                    "/v1/lanes?run={}",
                    urlencoding::encode(&run.subject)
                ))
                .await
            {
                print!("{}", render_run_lanes(&lanes, now));
            }
            Ok(())
        }
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
        MissionViewCommand::Publish(args) => {
            eprintln!("st: missions publish is legacy; use st apply FILE with the same options");
            publish_mission_file(client, args, json_output).await
        }
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases
        MissionViewCommand::Check(args) => check_mission_file(client, args, json_output).await,
        MissionViewCommand::Start(args) => start_mission_run(client, args, json_output).await,
        MissionViewCommand::Cancel(args) => {
            cancel_mission_run(client, endpoint, args, json_output).await
        }
        MissionViewCommand::Outcome(args) => {
            set_mission_run_outcome(client, args, json_output).await
        }
        MissionViewCommand::ReportTo(args) => {
            set_mission_run_report(client, args, json_output).await
        }
        MissionViewCommand::Retire(args) => retire_mission(client, args, json_output).await,
        MissionViewCommand::Queued { agent } => {
            show_agent_queue(endpoint, &agent, json_output).await
        }
        MissionViewCommand::Requests { subscription, all } => {
            list_subscription_requests(client, subscription, all, json_output).await
        }
        MissionViewCommand::Release(args) => {
            decide_subscription_request(client, "release", args, json_output).await
        }
        MissionViewCommand::CancelRequest(args) => {
            decide_subscription_request(client, "cancel", args, json_output).await
        }
    }
}

/// `st missions check FILE`: exit status 1 when a gate is broken.
async fn check_mission_file(
    client: &Client,
    args: MissionCheckArgs,
    json_output: bool,
) -> Result<()> {
    let (kdl, source_name) = read_intent(Some(&args.file))?;
    let intent = IntentInput { kdl, source_name };
    let mut announced = false;
    let checked = run_gate_check(
        client,
        &intent,
        &args.workspace,
        &args.inputs,
        |view, item| {
            if json_output {
                return;
            }
            if !announced {
                println!("{}", gate_check_heading(view));
                announced = true;
            }
            print!("{}", render_gate_check_item(item));
        },
    )
    .await?;
    let view = checked.context("this st daemon cannot check exec gates; update it")?;
    if json_output {
        print_value(&view, true)?;
    } else if view.gates.is_empty() {
        println!("No exec gates to check.");
    }
    if view.gates.iter().any(|item| item.answer == "broken") {
        return Err(CommandExit(1).into());
    }
    Ok(())
}

async fn cancel_mission_run(
    client: &Client,
    endpoint: &Endpoint,
    args: MissionCancelArgs,
    json_output: bool,
) -> Result<()> {
    let subject = resolve_member_subject(
        endpoint,
        &args.mission_run,
        "mission-run",
        Entity::MissionRun {
            unfinished_only: true,
        },
        completion::Matching::Exact,
    )
    .await?;
    let run: MissionRunView = client
        .get(&format!(
            "/v1/mission-runs/{}",
            urlencoding::encode(&subject)
        ))
        .await?;
    anyhow::ensure!(
        !matches!(run.status.as_str(), "completed" | "failed" | "cancelled"),
        "mission run `{subject}` is already {}",
        run.status
    );
    let actor = args.actor;
    let generated = generated_client(endpoint, Some(&actor))?;
    let capabilities = generated.capabilities().await?;
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let response = generated
        .mission_cancel(
            format!("action/{nonce}"),
            format!("mission-cancel:{subject}:{nonce}"),
            ClientFence {
                snapshot_id: capabilities.snapshot.id,
                mission_generation: Some(run.generation),
                ..ClientFence::default()
            },
            ClientTargetParameters {
                target_id: subject,
                reason: Some(args.reason),
                ..ClientTargetParameters::default()
            },
        )
        .await?;
    print_client_value(&response, json_output)
}

async fn set_mission_run_outcome(
    client: &Client,
    args: MissionOutcomeArgs,
    json_output: bool,
) -> Result<()> {
    let id = args
        .mission_run
        .strip_prefix("mission-run/")
        .unwrap_or(&args.mission_run);
    let subject = format!("mission-run/{id}");
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let run: MissionRunView = client
        .post(
            &format!("/v1/mission-runs/{}/outcome", urlencoding::encode(&subject)),
            &MissionRunOutcomeRequest {
                actor: args.actor,
                status: args.status,
                reason: args.reason,
                idempotency_key: format!("mission-outcome:{subject}:{nonce}"),
            },
        )
        .await?;
    if json_output {
        print_value(&run, true)
    } else {
        println!("{} is now {}", run.subject, run.status);
        Ok(())
    }
}

async fn set_mission_run_report(
    client: &Client,
    args: MissionReportToArgs,
    json_output: bool,
) -> Result<()> {
    let id = args
        .mission_run
        .strip_prefix("mission-run/")
        .unwrap_or(&args.mission_run);
    let subject = format!("mission-run/{id}");
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let report: MissionRunReportView = client
        .post(
            &format!("/v1/mission-runs/{}/report-to", urlencoding::encode(&subject)),
            &MissionRunReportRequest {
                actor: args.actor,
                report_to: args.agent,
                stalled_after_ms: args.stalled_after,
                report_stalls: args.report_stalls,
                report_completed: args.report_completed,
                idempotency_key: format!("mission-report-to:{subject}:{nonce}"),
            },
        )
        .await?;
    if json_output {
        return print_value(&report, true);
    }
    let unchanged = if report.changed { "" } else { " (unchanged)" };
    match &report.report_to {
        Some(agent) => println!(
            "{} reports to {agent}: failed, cancelled{}{}{unchanged}",
            report.run,
            if report.report_stalls {
                format!(
                    ", stalled after {}",
                    whole_duration(report.stalled_after_ms.unwrap_or_default())
                )
            } else {
                String::new()
            },
            if report.report_completed {
                ", completed"
            } else {
                ""
            },
        ),
        None => println!("{} reports to nobody{unchanged}", report.run),
    }
    Ok(())
}

async fn retire_mission(client: &Client, args: MissionRetireArgs, json_output: bool) -> Result<()> {
    let id = args
        .mission
        .strip_prefix("mission/")
        .unwrap_or(&args.mission);
    let mission = format!("mission/{id}");
    let nonce = uuid::Uuid::now_v7().simple().to_string();
    let retired: MissionSpec = client
        .post(
            &format!("/v1/missions/{}/retire", urlencoding::encode(id)),
            &MissionRetireRequest {
                actor: args.actor,
                idempotency_key: format!("mission-retire:{mission}:{nonce}"),
            },
        )
        .await?;
    if json_output {
        print_value(&retired, true)
    } else {
        println!(
            "{} is retired at revision {}",
            retired.subject, retired.revision
        );
        Ok(())
    }
}

fn print_mission_tree_continuation(
    selected: &MissionRunView,
    page: &st3::model::MissionRunTreePage,
    limit: usize,
) {
    if let Some(cursor) = &page.next_cursor {
        println!(
            "\nTREE      More runs follow; st missions show {} --cursor {cursor} --limit {limit}",
            selected.subject,
        );
    }
}

fn render_mission_overview(view: &Value) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "MISSION  {}\nRUNS     {} total\n",
        view["mission"].as_str().unwrap_or("?"),
        view["total_runs"]
    );
    if let Some(counts) = view["counts"].as_object() {
        for (state, count) in counts {
            let _ = writeln!(out, "  {state}: {count}");
        }
    }
    if let Some(revision) = view["revision"].as_str() {
        let _ = writeln!(out, "REVISION {revision}");
    }
    if let Some(value) = view.get("provenance")
        && let Ok(provenance) = serde_json::from_value(value.clone())
    {
        out.push_str(&st3::provenance::render(&provenance));
    }
    for (key, title) in [("newest", "NEWEST"), ("failed", "FAILED")] {
        let _ = writeln!(out, "{title} (up to {} runs)", view["preview_limit"]);
        if let Some(runs) = view[key].as_array() {
            for run in runs {
                let _ = writeln!(
                    out,
                    "  {}  {}",
                    run["id"].as_str().unwrap_or("?"),
                    run["status"].as_str().unwrap_or("?")
                );
                if let Some(reason) = run["reason"].as_str() {
                    let _ = writeln!(out, "    reason: {reason}");
                }
            }
        }
    }
    out
}

/// The lanes a mission run owns, for `st missions show`.
fn render_run_lanes(lanes: &[st3::model::LaneView], now: u128) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    if lanes.is_empty() {
        return output;
    }
    let _ = writeln!(output, "\nLANES");
    for lane in lanes {
        let _ = writeln!(
            output,
            "  {}  {} {}{}",
            lane.subject,
            lane.entries.len(),
            if lane.entries.len() == 1 {
                "entry"
            } else {
                "entries"
            },
            if lane.open { "" } else { " · closed" }
        );
        for entry in &lane.entries {
            render_lane_entry(
                &mut output,
                "    ",
                lane.entries_prefix.as_deref(),
                entry,
                now,
            );
        }
    }
    output
}

async fn put_document_bytes(
    client: &Client,
    name: String,
    bytes: Vec<u8>,
) -> Result<DocumentVersion> {
    let versions: DocumentListResponse = client
        .get(&format!(
            "/v1/documents?name={}",
            urlencoding::encode(&name)
        ))
        .await?;
    let expected_document = versions
        .items
        .iter()
        .find(|version| version.latest)
        .map(|version| version.binding_claim_id.clone());
    let hash = hex::encode(Sha256::digest(&bytes));
    client
        .post(
            "/v1/documents",
            &DocumentPutRequest {
                name: name.clone(),
                bytes,
                expected_document,
                idempotency_key: format!("document:{name}:{hash}"),
            },
        )
        .await
}

fn planning_feedback_intent(
    session_id: &str,
    operation_id: &str,
    document: &str,
    variant: &str,
) -> String {
    format!(
        "version 2\nplanning-session {session_id:?} {{\n  feedback {operation_id:?} {{\n    document {document:?}\n    variant {variant:?}\n  }}\n}}\n"
    )
}

fn planning_cancellation_intent(session_id: &str, operation_id: &str, reason: &str) -> String {
    format!(
        "version 2\nplanning-session {session_id:?} {{\n  cancellation {operation_id:?} {{\n    reason {reason:?}\n  }}\n}}\n"
    )
}

async fn list_subscription_requests(
    client: &Client,
    subscription: String,
    all: bool,
    json_output: bool,
) -> Result<()> {
    let subscription = if subscription.starts_with("subscription/") {
        subscription
    } else {
        format!("subscription/{subscription}")
    };
    let mut requests: Vec<SubscriptionRequestView> = client
        .get(&format!(
            "/v1/subscription-requests?subscription={}",
            urlencoding::encode(&subscription)
        ))
        .await?;
    if !all {
        requests.retain(|request| matches!(request.status.as_str(), "pending" | "held"));
    }
    if json_output {
        return print_value(&requests, true);
    }
    println!("REQUESTS  {}", requests.len());
    println!("SUBSCRIPTION  {subscription}");
    for request in &requests {
        println!(
            "{}  {}  {}{}",
            request.request,
            request.status,
            request.resource,
            request
                .mission_run
                .as_deref()
                .map(|run| format!("  {run}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

async fn decide_subscription_request(
    client: &Client,
    decision: &str,
    args: SubscriptionRequestArgs,
    json_output: bool,
) -> Result<()> {
    let response: SubscriptionRequestView = client
        .post(
            &format!(
                "/v1/subscription-requests/{decision}/{}",
                urlencoding::encode(&args.request)
            ),
            &SubscriptionRequestDecision {
                actor: args.actor,
                reason: args.reason,
                idempotency_key: format!(
                    "subscription-request-{decision}:{}:{}",
                    args.request,
                    uuid::Uuid::now_v7().simple()
                ),
            },
        )
        .await?;
    print_value(&response, json_output)
}
