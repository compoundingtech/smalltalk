use super::*;

/// `st driver-hook NAME [ARGS]`: answer one harness hook with the payload on stdin.
pub(super) fn run_driver_hook() -> ExitCode {
    let arguments = std::env::args().skip(2).collect::<Vec<_>>();
    let Some((name, rest)) = arguments.split_first() else {
        eprintln!(
            "usage: st {} NAME [ARGS]; NAME is one of {}",
            st3::driver_hook::SUBCOMMAND,
            st3::driver_hook::HOOKS.join(", ")
        );
        return ExitCode::from(2);
    };
    let env = st3::driver_hook::ProcessEnv;
    let run = || {
        st3::driver_hook::run(
            name,
            rest,
            &env,
            // Unlocked: the status-line tee reads stdin itself, and a held lock would deadlock it.
            &mut std::io::stdin(),
            &mut |diagnostic| {
                if let Err(error) = st3::driver_hook::post_diagnostic(&env, &diagnostic) {
                    eprintln!(
                        "st: could not record the {} diagnostic: {error:#}",
                        diagnostic.code
                    );
                }
            },
        )
    };
    let code = if name == "claude-observe" {
        st3::telemetry::hook(&env, run, st3::driver_hook::submit_telemetry)
    } else {
        st3::telemetry::local_only();
        run()
    };
    ExitCode::from(code)
}

async fn wait_for_agent_incarnation(client: &Client, actor: &str) -> Result<String> {
    wait_for_agent_incarnation_from(client, actor, has_local_pty_registry()).await
}

pub(super) async fn run_driver(client: &Client, args: DriverArgs, catalog: Option<&Path>) -> Result<()> {
    if args.seed.is_some() {
        anyhow::ensure!(
            args.driver == "omp"
                && st3::native_resume::selector_scope(&args.driver, &args.argv).is_none(),
            "native seed cannot accompany an authored session selector"
        );
        for variable in [
            st3::suspension::RESUME_ENV,
            st3::suspension::CONTINUE_ENV,
            st3::suspension::CONTINUE_PATH_ENV,
        ] {
            anyhow::ensure!(
                std::env::var_os(variable).is_none(),
                "native seed cannot accompany resume environment ({variable})"
            );
        }
    }
    if args.driver == "claude-mcp" {
        anyhow::ensure!(
            args.argv.is_empty(),
            "the Claude channel takes no provider argv"
        );
        let fallback_subject = std::env::var("ST3_SUBJECT").ok();
        let Some(subject) = args
            .subject
            .as_deref()
            .or(fallback_subject.as_deref())
            .filter(|subject| !subject.trim().is_empty())
        else {
            anyhow::ensure!(
                args.identity.is_none(),
                "the Claude channel identity requires a subject"
            );
            return st3::claude_channel::run_idle().await;
        };
        let identity = subject.strip_prefix("agent/").unwrap_or(subject);
        let paths = match st_drivers::driver_paths::Paths::from_environment(identity, &|name| {
            std::env::var(name).ok()
        })? {
            Some(paths) => NativePaths::from_resolved(subject, paths),
            None => NativePaths::legacy(subject, "claude")?,
        };
        if push_mailbox_enabled() {
            return st3::claude_channel::run(
                client,
                subject,
                wait_for_agent_incarnation(client, subject),
                &paths.resolved(),
                &paths.identity,
                &paths.runtime_id,
            )
            .await;
        }
        return st_drivers::claude_mcp::run_st3(&paths.driver_root, &paths.identity);
    }
    if matches!(args.driver.as_str(), "pi-channel" | "omp-channel") {
        let identity = args
            .identity
            .as_deref()
            .context("the pi-family channel has no identity")?;
        anyhow::ensure!(
            args.argv.is_empty(),
            "the pi-family channel takes no provider argv"
        );
        let driver = if args.driver == "omp-channel" {
            "omp"
        } else {
            "pi"
        };
        let subject = normalize_agent_subject(identity);
        let paths = st_drivers::driver_paths::Paths::from_environment(
            subject.strip_prefix("agent/").unwrap_or(&subject),
            &|name| std::env::var(name).ok(),
        )?;
        let root = paths
            .as_ref()
            .map(|paths| paths.root.as_path())
            .or(catalog)
            .context("the pi-family channel has no driver root")?;
        return run_pi_channel(client, &normalize_agent_subject(identity), driver, root).await;
    }
    let subject = args
        .subject
        .as_deref()
        .context("the driver has no subject")?;
    if st3::skill::HARNESSES.contains(&args.driver.as_str()) {
        // A seat starts idle: a declaration stored before st dropped the startup prompt still
        // carries it, and it would start a turn nobody asked for.
        let mut argv = args.argv;
        st3::boot::strip_legacy_prompt(&args.driver, &mut argv);
        // Install the skill from this binary, so it describes the commands this driver serves.
        // A seat without it still runs; the failure is logged beside the driver's other warnings.
        if let Err(error) = st3::skill::install(&args.driver) {
            let _ = write_driver_log(
                subject,
                &format!(
                    "could not install the st skill for {}: {error:#}",
                    args.driver
                ),
            );
        }
        // Harness-session state (OpenCode delivery ledgers, Claude resume bindings) stays beneath
        // st3's driver directory, never st2's; the seat's hooks choose the same root.
        if let Some(drivers) = std::env::var_os("ST3_DRIVER_STATE_DIR") {
            st_drivers::run::use_harness_state_root(PathBuf::from(drivers).join("sessions"));
        }
        if let Some(state) = st_drivers::reexec::resume_path(st_drivers::reexec::DRIVER_RESUME_ENV) {
            return resume_native_driver(client, subject, &args.driver, argv, &state).await;
        }
        let incarnation = wait_for_agent_incarnation(client, subject).await?;
        let paths = NativePaths::prepare(subject, &args.driver)?;
        let sessions = paths.session_dir.join("provider-sessions");
        if let Some(session) = st3::native_seed::first_launch(
            client, subject, &incarnation, &args.driver, args.seed.as_deref(), &sessions,
            st3::native_resume::requested().is_some(),
        ).await? {
            argv = st3::native_resume::pi_family_argv("omp", argv, &sessions, &session, None)
                .map_err(|refusal| anyhow::anyhow!("{}: {}", refusal.code, refusal.reason))?;
        }
        if let (Some(message), Some(id)) = (&args.initial_message, &args.initial_message_id) {
            // The durable launch receipt precedes invocation. A fresh incarnation never repeats
            // the first message; adoption resumes above without invoking a new provider.
            let incarnation = wait_for_agent_incarnation(client, subject).await?;
            if retry_while_daemon_unreachable(subject, || {
                st3::creation::claim_initial_message(client, subject, id, &incarnation)
            })
            .await?
            {
                st3::creation::append_native_message(&args.driver, &mut argv, message)?;
            }
        }
        if args.driver == "codex" {
            return run_codex_native(client, subject, argv).await;
        }
        return run_st2_native_driver(client, subject, &args.driver, argv, paths).await;
    }
    let (program, arguments) = args.argv.split_first().context("driver argv is empty")?;
    let mut child = tokio::process::Command::new(program)
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start {} provider", args.driver))?;
    let status = child.wait().await?;
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt as _;
        status.signal()
    };
    let exit = ClaimInput {
        subject: subject.into(),
        kind: "runtime.observed".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("status".into(), Value::String("exited".into())),
            (
                "exit_code".into(),
                status.code().map(Value::from).unwrap_or(Value::Null),
            ),
            (
                "exit_signal".into(),
                signal.map(Value::from).unwrap_or(Value::Null),
            ),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: None,
    };
    // A provider that exits while the daemon restarts still reports its own exit status.
    let _: ClaimRecord =
        retry_while_daemon_unreachable(subject, || client.post("/v1/claims", &exit)).await?;
    if args.driver == "exec" {
        let code = status
            .code()
            .unwrap_or_else(|| 128_i32.saturating_add(signal.unwrap_or(1)))
            .clamp(0, 255) as u8;
        if code != 0 {
            return Err(CommandExit(code).into());
        }
        return Ok(());
    }
    anyhow::ensure!(status.success(), "{} exited with {status}", args.driver);
    Ok(())
}

async fn run_st2_native_driver(
    client: &Client,
    subject: &str,
    driver: &str,
    argv: Vec<String>,
    paths: NativePaths,
) -> Result<()> {
    anyhow::ensure!(!argv.is_empty(), "the {driver} driver argv is empty");
    if driver == "claude" {
        reject_noninteractive_claude_argv(&argv)?;
    }
    #[cfg(unix)]
    if matches!(driver, "pi" | "omp")
        && let Err(skip) = st3::native_resume::pi_family_link_transcript(
            &argv,
            &paths.session_dir.join("provider-sessions"),
        )
    {
        let _ = write_driver_log(
            subject,
            &json!({"type":"authored_resume_link_skipped","driver":driver,"code":skip.code,"reason":skip.reason}).to_string(),
        );
    }
    let incarnation = wait_for_agent_incarnation(client, subject).await?;
    // A driver launched while the daemon restarts waits for it; exiting here would end the seat.
    retry_while_daemon_unreachable(subject, || {
        publish_harness_state(
            client,
            subject,
            driver,
            "starting",
            Some(&incarnation),
            None,
        )
    })
    .await?;
    let select = |argv: Vec<String>, session: &str, path: Option<&Path>| -> Result<_> {
        Ok(match driver {
            "claude" => st3::native_resume::claude_argv(
                argv,
                session,
                &std::env::current_dir()?,
                st3::native_resume::claude_home().as_deref(),
            ),
            "pi" | "omp" => st3::native_resume::pi_family_argv(
                driver,
                argv,
                &paths.session_dir.join("provider-sessions"),
                session,
                path,
            ),
            "opencode" => st3::native_resume::opencode_argv(
                argv,
                session,
                st3::native_resume::opencode_data_dir().as_deref(),
            ),
            _ => unreachable!("the native driver was checked"),
        })
    };
    // A resumed seat relaunches its harness on the session it suspended on, or not at all. Any
    // other relaunch continues the seat's last session when the harness can, or starts anew.
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
    let mut selected_session = None;
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
    let argv = if let Some(session) = st3::native_resume::requested() {
        if driver == "claude" {
            let path = std::env::var_os(st3::rollout::RESUME_PATH_ENV).map(PathBuf::from);
            if let Err(refusal) = st3::native_resume::claude_carry_transcript(
                &session,
                &std::env::current_dir()?,
                st3::native_resume::claude_home().as_deref(),
                path.as_deref(),
            ) {
                return Err(refuse_native_resume(
                    client,
                    subject,
                    &incarnation,
                    driver,
                    st3::native_resume::Refusal::new("transcript-copy-failed", refusal.to_string()),
                )
                .await);
            }
        }
        let path = std::env::var_os(st3::rollout::RESUME_PATH_ENV).map(PathBuf::from);
        match select(argv, &session, path.as_deref())? {
            Ok(argv) => {
                // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
                selected_session = Some(session);
                // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
                argv
            }
            Err(refusal) => {
                return Err(
                    refuse_native_resume(client, subject, &incarnation, driver, refusal).await,
                );
            }
        }
    } else if let Some((session, path)) = st3::native_resume::continued() {
        if driver == "claude" {
            // A seat whose workspace changed finds its transcript under the earlier one.
            let _ = st3::native_resume::claude_carry_transcript(
                &session,
                &std::env::current_dir()?,
                st3::native_resume::claude_home().as_deref(),
                path.as_deref(),
            );
        }
        // A saved continuation path is not the strict import bootstrap selector.
        // OMP restarts keep selecting their session by ID in the managed inventory.
        match select(argv.clone(), &session, None)? {
            Ok(argv) => {
                // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
                selected_session = Some(session);
                // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
                argv
            }
            Err(refusal) => {
                skip_native_continue(client, subject, &incarnation, driver, &session, refusal)
                    .await;
                argv
            }
        }
    } else {
        argv
    };
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
    let mut provider_environment = Vec::new();
    if driver == "omp" {
        let sessions = paths.session_dir.join("provider-sessions");
        if let Some(tool_call_id) =
            st3::omp_ask_resume::pending_for_selected(&sessions, selected_session.as_deref())
        {
            // Fail open: bridge I/O or diagnostics must never prevent native continuation.
            match st3::omp_ask_resume::heal(
                &argv[0],
                &sessions,
                selected_session.as_deref().expect("pending ask has a selected session"),
            )
            .await
            {
                Ok(output) => {
                    let _ = write_driver_log(subject, &output);
                    provider_environment.push(("ST3_OMP_PENDING_ASK".into(), tool_call_id));
                }
                Err(error) => {
                    let _ = write_driver_log(subject, &json!({"type":"omp_pending_ask_heal_failed","error":format!("{error:#}")}).to_string());
                }
            }
        }
    }
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
    if driver == "opencode" {
        // The predecessor's bound session is not this launch's, and the driver reports this file.
        let _ = fs::remove_file(
            paths
                .agent_dir
                .join(st_drivers::opencode_session::NATIVE_SESSION_FILE),
        );
    }
    st_drivers::harness_events::enable(&paths.agent_dir, &incarnation)?;
    let harness_state_path = st_drivers::harness_state::harness_state_path(&paths.agent_dir);
    let loop_state = NativeLoopState {
        paths: Some(paths.resolved()),
        predecessor_harness_record: fs::read(&harness_state_path).ok(),
        ..NativeLoopState::default()
    };
    let task = spawn_st3_provider(
        driver,
        &paths,
        ProviderStart::Launch(
            argv,
            // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
            provider_environment,
            // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
        ),
    );
    drive_st2_native(
        client,
        subject,
        driver,
        paths,
        incarnation,
        loop_state,
        task,
    )
    .await
}

enum ProviderStart {
    Launch(
        Vec<String>,
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
        Vec<(String, String)>,
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
    ),
    Adopt(st_drivers::provider_session::DetachedSession),
}

fn spawn_st3_provider(
    driver: &str,
    paths: &NativePaths,
    start: ProviderStart,
) -> tokio::task::JoinHandle<Result<()>> {
    use st_drivers::provider_session::DetachedSession;
    let paths = paths.clone();
    let driver = driver.to_owned();
    if push_mailbox_enabled() && driver == "opencode" {
        st_drivers::push_mailbox::register(&paths.agent_dir);
    }
    tokio::task::spawn_blocking(move || {
        let outcome = match start {
        ProviderStart::Launch(
            argv,
            // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
            environment,
            // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
        ) => match driver.as_str() {
            "claude" => st_drivers::claude_session::run_controlled_paths(
                &paths.resolved(),
                paths.identity,
                paths.runtime_id,
                argv,
            ),
            "pi" => st_drivers::pi_session::run_native(
                &paths.resolved(),
                paths.identity,
                paths.runtime_id,
                argv,
            ),
            "omp" => st_drivers::omp_session::run_native(
                &paths.resolved(),
                paths.identity,
                paths.runtime_id,
                argv,
                // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
                &environment,
                // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
            ),
            "opencode" => st_drivers::opencode_session::run_with_paths(
                &paths.resolved(),
                paths.identity,
                paths.runtime_id,
                argv,
                st_drivers::session_control::SessionControl::Graph(paths.delivery_gate),
            ),
            _ => unreachable!("the native driver was checked"),
        },
        ProviderStart::Adopt(session) => match (driver.as_str(), session) {
            ("claude", DetachedSession::Provider { pid, session, seq }) => {
                st_drivers::claude_session::adopt_controlled_paths(
                    &paths.resolved(),
                    &paths.identity,
                    &paths.runtime_id,
                    pid,
                    &session,
                    seq,
                )
            }
            ("pi", DetachedSession::Provider { pid, session, seq }) => {
                st_drivers::pi_session::adopt_native(
                    &paths.resolved(),
                    paths.identity,
                    paths.runtime_id,
                    pid,
                    session,
                    seq,
                )
            }
            ("omp", DetachedSession::Provider { pid, session, seq }) => {
                st_drivers::omp_session::adopt_native(
                    &paths.resolved(),
                    paths.identity,
                    paths.runtime_id,
                    pid,
                    session,
                    seq,
                )
            }
            (
                "opencode",
                DetachedSession::OpenCode {
                    pid,
                    session,
                    seq,
                    port,
                    password,
                    version_ok,
                    producer_version,
                },
            ) => st_drivers::opencode_session::adopt_with_paths(
                &paths.resolved(),
                paths.identity,
                paths.runtime_id,
                pid,
                session,
                seq,
                port,
                password,
                version_ok,
                producer_version,
                st_drivers::session_control::SessionControl::Graph(paths.delivery_gate),
            ),
            (driver, session) => {
                anyhow::bail!("a {driver} driver cannot adopt this provider session: {session:?}")
            }
        },
        };
        #[cfg(feature = "test-support")]
        if env!("CARGO_BIN_NAME") == "st3-fixture" {
            st3::test_support::hold_provider_completion(&outcome)?;
        }
        outcome
    })
}

/// Resume a native driver a predecessor image re-executed into this binary.
async fn resume_native_driver(
    client: &Client,
    subject: &str,
    driver: &str,
    argv: Vec<String>,
    state_path: &Path,
) -> Result<()> {
    let mut resume: DriverResume = st_drivers::reexec::read_state(state_path)?;
    anyhow::ensure!(
        resume.subject == subject && resume.driver == driver,
        "the resume state belongs to the {} driver of `{}`, not the {driver} driver of `{subject}`",
        resume.driver,
        resume.subject
    );
    resume.loop_state.harness_record_started = true;
    resume.loop_state.settle = ResumeSettle::resumed();
    let _ = write_driver_log(
        subject,
        &format!(
            "the {driver} driver resumed in {} after its st binary was replaced; the provider kept running",
            st_drivers::reexec::installed_binary()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "a replacement binary".into())
        ),
    );
    if driver == "codex" {
        return drive_codex_native(
            client,
            subject,
            argv,
            resume.incarnation,
            ProviderStart::Adopt(resume.session),
            resume.loop_state,
        )
        .await;
    }
    let mut paths = NativePaths::resumed(subject, driver, resume.loop_state.paths.clone())?;
    if driver == "opencode" {
        paths.pending_hold_adoption = legacy_delivery_hold(subject, &paths.agent_dir);
    }
    resume.loop_state.paths = Some(paths.resolved());
    let task = spawn_st3_provider(driver, &paths, ProviderStart::Adopt(resume.session));
    drive_st2_native(
        client,
        subject,
        driver,
        paths,
        resume.incarnation,
        resume.loop_state,
        task,
    )
    .await
}

/// The detached session a provider task returned, if it returned one.
fn detached_session(outcome: &Result<()>) -> Option<st_drivers::provider_session::DetachedSession> {
    outcome
        .as_ref()
        .err()?
        .downcast_ref::<st_drivers::provider_session::Detached>()
        .map(|detached| detached.session.clone())
}

async fn drive_st2_native(
    client: &Client,
    subject: &str,
    driver: &str,
    mut paths: NativePaths,
    incarnation: String,
    mut loop_state: NativeLoopState,
    mut task: tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let settle = loop_state.settle;
    let NativePaths {
        session_dir,
        agent_dir,
        identity,
        runtime_id,
        ..
    } = paths.clone();
    let mut mailbox =
        match NativeMailbox::start(client, subject, &incarnation, driver, &mut loop_state).await {
            Ok(mailbox) => mailbox,
            // A provider can run its whole life inside the bind's one-second retry pause (a
            // loaded host, a short fixture). The daemon then refuses the binding because that
            // provider has ended. There is nothing left to deliver to: record the exit as
            // usual instead of failing the wrapper with the refusal.
            Err(error) if provider_ended_before_bind(&error) && provider_task_settled(&task).await => {
                NativeMailbox::unbound(subject, &incarnation, driver, &mut loop_state)
            }
            Err(error) => return Err(error),
        };
    let attach_started = settle.attach_started();
    if driver == "claude" && mailbox.subscription.is_some() && settle.passed()
        && let Err(error) = check_claude_attachment(
            client, subject, &incarnation, &mailbox, attach_started, &mut loop_state,
        ).await
    {
        let _ = write_driver_log(subject, &format!("Claude attachment check will retry: {error:#}"));
    }
    let mut observations = NativeObservations::start(&agent_dir, &incarnation)?.settle(settle);
    loop_state.claude_readiness_fallback.native_state_seen |= observations.native_state_seen;
    let harness_state_path = st_drivers::harness_state::harness_state_path(&agent_dir);
    let inbox = st_drivers::message::inbox_dir(&agent_dir);
    let archive = st_drivers::message::archive_dir(&agent_dir);
    // This tick retries receipts and native handoffs. Push delivery reads its cached mailbox;
    // only an already-running legacy seat still polls durable history.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut work_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    work_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renewed_minute = None;
    let mut work_retry = st3::client::DriverRetry::default();
    let mut control_retry = st3::client::DriverRetry::default();
    let mut last_activity_fingerprint = None;
    let mut last_usage_fingerprint = None;
    let mut last_limits_fingerprint = None;
    let mut last_control_warning = None;
    let mut last_admission_fingerprint = None;
    let mut last_capacity_fingerprint = None;
    let mut delivery = NativeDeliverySupervisor::resumed(loop_state.delivery_episode);
    let mut replacement = DriverReplacement::new();
    let mut binding_watch = ClaudeBindingWatch::default();
    let mut reported_session = None;
    // Claude's hooks and omp's extension keep the subagent ledger; this driver records it on the
    // seat.
    let mut subagents = matches!(driver, "claude" | "omp").then(|| {
        st3::subagents::Publisher::start(
            subject,
            driver,
            &incarnation,
            &agent_dir,
            st_drivers::subagents::now_ms(),
        )
    });
    let mut completion_announced = false;
    loop {
        #[cfg(feature = "test-support")]
        {
            completion_announced |= fixture_terminal_completion_barrier(
                &mut observations, client, subject, driver, &mut loop_state.ready, &task,
            ).await?;
            if completion_announced && env!("CARGO_BIN_NAME") == "st3-fixture"
                && let Some(root) = std::env::var_os("ST3_FIXTURE_TERMINAL_COMPLETION")
            {
                fs::write(PathBuf::from(root).join("awaiting-completion"), b"awaiting")?;
            }
        }
        tokio::select! {
            frame = mailbox.recv() => {
                #[cfg(feature = "test-support")]
                if env!("CARGO_BIN_NAME") == "st3-fixture"
                    && matches!(&frame, Some(st3::mailbox::Frame::Fenced { .. }))
                    && let Some(root) = std::env::var_os("ST3_FIXTURE_TERMINAL_COMPLETION")
                {
                    fs::write(PathBuf::from(root).join("fence-received"),
                        if task.is_finished() { "finished" } else { "pending" })?;
                }
                // The subscription can reopen after the provider has ended, and the daemon then
                // fences it as terminal. With the provider's task returned there is nothing left
                // to deliver to: stop listening and let the task arm record the exit.
                if let Some(st3::mailbox::Frame::Fenced { reason }) = &frame
                    && provider_ended_refusal(reason)
                    && provider_task_settled(&task).await
                {
                    mailbox.subscription = None;
                    continue;
                }
                let mail_changed = matches!(&frame, Some(st3::mailbox::Frame::Mailbox { .. }));
                mailbox.accept(frame, &runtime_id)?;
                if driver == "opencode" && mail_changed
                    && let Err(error) = sync_native_delivery_control(client, subject, &mut paths, &mailbox, &inbox, "opencode-server").await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
            }
            wake = observations.recv(), if !completion_announced => {
                wake?;
                match observations.drain_live(client, subject, driver, &mut loop_state.ready).await {
                    Ok(ended) => completion_announced |= ended,
                    Err(error) => note_driver_tick_failure(subject, error, &mut last_control_warning),
                }
            }
            result = &mut task, if completion_announced || fixture_completion_task_enabled() => {
                loop_state.claude_readiness_fallback.native_state_seen |= observations.native_state_seen;
                let outcome = result?;
                if let Some(session) = detached_session(&outcome) {
                    loop_state.delivery_episode = delivery.episode;
                    let resume = DriverResume {
                        driver: driver.to_owned(),
                        subject: subject.to_owned(),
                        incarnation: incarnation.clone(),
                        session: session.clone(),
                        loop_state,
                    };
                    let _ = replacement.exec(subject, &paths.state_root(), &resume);
                    loop_state = resume.loop_state;
                    task = spawn_st3_provider(driver, &paths, ProviderStart::Adopt(session));
                    completion_announced = false;
                    continue;
                }
                finish_native_exit_report(subject, async {
                    // A refused omp never reaches the session loop. Publish its durable named
                    // admission boundary before recording exit, retrying through daemon outages.
                    if matches!(driver, "omp" | "opencode") {
                        let observed = st_drivers::driver_diagnostic::read(
                            &st_drivers::driver_diagnostic::path(&agent_dir));
                        if let Some(input) = admission_diagnostic(subject, &incarnation, &observed) {
                            let _: ClaimRecord = retry_while_daemon_unreachable(subject, || {
                                client.post("/v1/claims", &input)
                            }).await?;
                        }
                    }
                    if let Err(error) = observations.drain(client, subject, driver, &mut loop_state.ready).await {
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                    // The harness is gone, and its subagents with it. The reconciler ends any this
                    // cannot record once it sees the runtime exit.
                    if let Some(subagents) = subagents.as_mut() {
                        let _ = tokio::time::timeout(
                            Duration::from_secs(5),
                            subagents.end_all(client, "harness-exited", "its harness exited"),
                        )
                        .await;
                    }
                    loop {
                        let result: Result<ClaimRecord> = client.post("/v1/claims", &ClaimInput {
                            subject: subject.into(),
                            kind: "runtime.observed".into(),
                            actor: Some(subject.into()),
                            fields: BTreeMap::from([
                                ("status".into(), Value::String("exited".into())),
                                ("runtime_id".into(), Value::String(runtime_id.clone())),
                                (
                                    "incarnation_id".into(),
                                    Value::String(incarnation.clone()),
                                ),
                                ("exit_code".into(), Value::from(if outcome.is_ok() { 0 } else { 1 })),
                            ]),
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: Some(native_exit_key(subject, &runtime_id, &incarnation)),
                        }).await;
                        match result {
                            Ok(_) => return Ok(()),
                            Err(error) => {
                                tolerate_driver_api_outage(subject, error, &mut last_control_warning)?;
                                tokio::time::sleep(Duration::from_millis(250)).await;
                            }
                        }
                    }
                }).await?;
                return outcome;
            }
            settled = settle.control_tick(&mut control_retry, &mut interval), if !completion_announced => {
                if driver == "claude" && mailbox.subscription.is_some() && settled
                    && let Err(error) = check_claude_attachment(
                        client, subject, &incarnation, &mailbox, attach_started, &mut loop_state,
                    ).await
                {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                if let Err(error) = observations.expire_due() {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }

                if observations.retry_due() {
                    match observations.drain_live(client, subject, driver, &mut loop_state.ready).await {
                        Ok(ended) => completion_announced |= ended,
                        Err(error) => note_driver_tick_failure(subject, error, &mut last_control_warning),
                    }
                    if completion_announced { continue; }
                }

                if driver == "claude" && mailbox.subscription.is_some() && settled {
                    match publish_claude_readiness_fallback(
                        client, &mailbox, &mut observations, &mut loop_state, current_unix_ms()? as u64,
                    ).await {
                        Ok(ended) => completion_announced |= ended,
                        Err(error) => note_driver_tick_failure(subject, error, &mut last_control_warning),
                    }
                    if completion_announced { continue; }
                }

                if driver == "opencode" && mailbox.subscription.is_some() {
                    if let Err(error) = sync_native_delivery_control(client, subject, &mut paths, &mailbox, &inbox, "opencode-server").await {
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                }
                let provider_incarnation = if observations.enabled {
                    observations.provider_incarnation.clone()
                } else {
                let current_record = fs::read(&harness_state_path).ok();
                loop_state.harness_record_started = harness_record_belongs_to_current_session(
                    loop_state.harness_record_started,
                    loop_state.predecessor_harness_record.as_deref(),
                    current_record.as_deref(),
                );
                if loop_state.harness_record_started {
                    st_drivers::harness_state::read(&harness_state_path, None)
                        .and_then(|observed| observed.evidence_incarnation)
                } else {
                    None
                }
                };
                let bound = match driver {
                    // Claude writes the transcript a resume needs only with its first turn.
                    "claude" => provider_incarnation
                        .as_deref()
                        .and_then(|token| st3::hooks::claude_binding(&agent_dir, token))
                        .filter(|session| {
                            let workspace = std::env::current_dir().unwrap_or_default();
                            st3::native_resume::claude_home().is_some_and(|home| {
                                st3::native_resume::claude_transcript(&home, &workspace, session)
                                    .is_file()
                            })
                        }),
                    "opencode" => st3::native_resume::opencode_bound_session(&agent_dir),
                    _ => None,
                };
                if let Some(session) = bound.filter(|_| settled)
                    && let Err(error) = report_native_session(
                        client,
                        subject,
                        &incarnation,
                        driver,
                        &session,
                        None,
                        &mut reported_session,
                    )
                    .await
                {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                if driver == "claude" && settled
                    && let Some(reason) = binding_watch.overdue(
                        &agent_dir,
                        provider_incarnation.as_deref(),
                        Instant::now(),
                    )
                {
                    let session = provider_incarnation.clone().unwrap_or_default();
                    let posted: Result<ClaimRecord> = client.post("/v1/claims", &ClaimInput {
                        subject: subject.into(),
                        kind: "harness.diagnostic".into(),
                        actor: Some(subject.into()),
                        fields: BTreeMap::from([
                            ("severity".into(), Value::String("error".into())),
                            ("status".into(), Value::String("failed".into())),
                            ("code".into(), Value::String(st3::driver_hook::UNBOUND_CODE.into())),
                            ("reason".into(), Value::String(reason)),
                            ("incarnation_id".into(), Value::String(incarnation.clone())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!(
                            "{}:{subject}:{session}",
                            st3::driver_hook::UNBOUND_CODE
                        )),
                    }).await;
                    if let Err(error) = posted {
                        binding_watch.retry();
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                }
                // Delivery runs first and on its own: a failing observation publish must never
                // hold back a message.
                if mailbox.subscription.is_some() {
                    if driver == "opencode" {
                        if let Err(error) = mailbox.pump(client, &agent_dir,
                            NativeDeliveryReceipts::OpenCode { session_dir: &session_dir, identity: &identity, runtime_id: &runtime_id }).await {
                            note_driver_tick_failure(subject, error, &mut last_control_warning);
                        }
                    }
                } else {
                if driver == "claude" {
                    delivery.report = Some(native_delivery_report("claude-channel", Some(&agent_dir)));
                    supervise_native_delivery(
                        client,
                        subject,
                        &inbox,
                        &archive,
                        "claude-channel",
                        NativeDeliveryReceipts::ClaudeChannel {
                            agent_dir: &agent_dir,
                            incarnation: claude_receipt_incarnation(&incarnation, provider_incarnation.as_deref()),
                        },
                        &incarnation,
                        &mut delivery,
                    )
                    .await;
                } else if driver == "opencode" {
                    delivery.report = Some(native_delivery_control_report("opencode-server", &paths.delivery_gate).to_string());
                    supervise_native_delivery(
                        client,
                        subject,
                        &inbox,
                        &archive,
                        "opencode-server",
                        NativeDeliveryReceipts::OpenCode {
                            session_dir: &session_dir,
                            identity: &identity,
                            runtime_id: &runtime_id,
                        },
                        &incarnation,
                        &mut delivery,
                    )
                    .await;
                }
                }
                // The older mailbox projection can create the first inbox file in this pass.
                // Read control now so an idle gate adds no extra polling interval to delivery.
                if driver == "opencode" && mailbox.subscription.is_none()
                    && let Err(error) = sync_native_delivery_control(client, subject, &mut paths, &mailbox, &inbox, "opencode-server").await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                if settled && control_retry.ready() {
                let tick: Result<()> = async {
                    if observations.enabled || !settled { return Ok(()) }
                    // The wrapper writes admission before claiming a new harness session. Wait
                    // for that claim so a predecessor's record cannot fence a successor.
                    if matches!(driver, "omp" | "opencode") && loop_state.harness_record_started {
                        publish_admission_diagnostic(client, subject, &incarnation, &agent_dir,
                            &mut last_admission_fingerprint).await?;
                    }
                    if native_file_may_override_channel(driver)
                        && loop_state.harness_record_started
                        && let Some(observed) = st_drivers::harness_state::read(&harness_state_path, None)
                    {
                        // A session claim is a startup fence, not an observation. Preserve the
                        // explicit `starting` state until a native observation supplies positive
                        // evidence. Push Claude startup readiness has a separate grace fallback;
                        // publishing the derived `claimed`
                        // indeterminacy would erase the more precise lifecycle state.
                        let claim_placeholder =
                            observed.state == st_drivers::harness_state::Activity::Unknown
                                && observed.reason.as_deref() == Some("claimed");
                        if !claim_placeholder {
                            if !loop_state.ready
                                && !matches!(
                                    observed.state,
                                    st_drivers::harness_state::Activity::Unknown
                                        | st_drivers::harness_state::Activity::Ended
                                )
                            {
                                let _: ClaimRecord = client.post("/v1/claims", &ClaimInput {
                                    subject: subject.into(),
                                    kind: "harness.observed".into(),
                                    actor: Some(subject.into()),
                                    fields: with_quiescence(BTreeMap::from([
                                        ("state".into(), Value::String("ready".into())),
                                        ("driver".into(), Value::String(driver.into())),
                                        (
                                            "transport".into(),
                                            Value::String(if driver == "claude" {
                                                "claude-channel".into()
                                            } else {
                                                "native".into()
                                            }),
                                        ),
                                        ("incarnation_id".into(), Value::String(incarnation.clone())),
                                    ])),
                                    evidence: Vec::new(),
                                    expected_subject: None,
                                    idempotency_key: Some(format!("native-ready:{subject}:{driver}:{incarnation}")),
                                }).await?;
                                loop_state.ready = true;
                            }
                            publish_harness_activity(
                                &ObservationClient { client, event: None },
                                subject,
                                driver,
                                if driver == "claude" {
                                    "claude-channel"
                                } else {
                                    "native"
                                },
                                Some(&incarnation),
                                &observed,
                                &mut last_activity_fingerprint,
                            )
                            .await?;
                            if observed.reason.as_deref() == Some("providerCapacity") {
                                let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
                                    driver,
                                    observed.since_ms,
                                    observed.reason.as_deref(),
                                ))?));
                                if last_capacity_fingerprint.as_deref() != Some(fingerprint.as_str()) {
                                    publish_provider_capacity_diagnostic(
                                        client,
                                        subject,
                                        &incarnation,
                                        observed.since_ms,
                                        &fingerprint,
                                    )
                                    .await?;
                                    last_capacity_fingerprint = Some(fingerprint);
                                }
                            } else {
                                last_capacity_fingerprint = None;
                            }
                            if let Some(context) = st_drivers::harness_context::read(
                                &st_drivers::harness_context::harness_context_path(&agent_dir)) {
                    publish_harness_usage(
                                &ObservationClient { client, event: None },
                                subject,
                                driver,
                                &incarnation,
                                &context,
                                &mut last_usage_fingerprint,
                            )
                            .await?;
                            publish_harness_limits(
                                &ObservationClient { client, event: None },
                                subject,
                                driver,
                                &incarnation,
                                &context,
                                &mut last_limits_fingerprint,
                            )
                            .await?;
                            }
                        }
                    }
                    publish_harness_timeline(
                        client,
                        subject,
                        driver,
                        &incarnation,
                        provider_incarnation.as_deref(),
                        &agent_dir,
                        &mut loop_state.published_timeline,
                    )
                    .await?;
                    Ok(())
                }.await;
                match tick {
                    Ok(()) => control_retry.succeeded(),
                    Err(error) => {
                        control_retry.failed(&error);
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                }
                }
                if let Some(subagents) = subagents.as_mut().filter(|_| settled)
                    && let Err(error) = subagents.tick(client).await
                {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                replacement.check();
            }
            _ = settle.retry_tick(&mut work_retry, &mut work_interval), if !completion_announced && settle.passed() => {
                let tick: Result<()> = async {
                    if let Some(minute) = renewal_minute(subject)?
                        && renewed_minute != Some(minute)
                        && work_retry.ready()
                    {
                        renew_claimed_work(client, subject, minute, &mut work_retry).await?;
                        work_retry.succeeded();
                        renewed_minute = Some(minute);
                    }
                    Ok(())
                }.await;
                match tick {
                    Ok(()) => {}
                    Err(error) => {
                        work_retry.failed(&error);
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                }
            }
        }
    }
}

// Fixture controls force the actual stream rejection branch before disposing the result.
// The installed executable always enables the provider completion branch.
fn fixture_completion_task_enabled() -> bool {
    #[cfg(feature = "test-support")]
    if env!("CARGO_BIN_NAME") == "st3-fixture"
        && let Some(root) = std::env::var_os("ST3_FIXTURE_TERMINAL_COMPLETION").map(PathBuf::from)
        && root.join("join-phase").exists()
        && !root.join("fence-received").exists()
    {
        return false;
    }
    true
}

// Only the separately compiled fixture executable can schedule this control.
// It holds no production provider, changes no claim or fence, and has a finite deadline.
#[cfg(feature = "test-support")]
async fn fixture_terminal_completion_barrier(
    observations: &mut NativeObservations,
    client: &Client,
    subject: &str,
    driver: &str,
    ready: &mut bool,
    task: &tokio::task::JoinHandle<Result<()>>,
) -> Result<bool> {
    if env!("CARGO_BIN_NAME") != "st3-fixture" || driver != "claude" {
        return Ok(false);
    }
    let Some(root) = std::env::var_os("ST3_FIXTURE_TERMINAL_COMPLETION").map(PathBuf::from) else {
        return Ok(false);
    };
    if root.join("observation-drained").exists()
        || !st_drivers::harness_state::read(
            &st_drivers::harness_state::harness_state_path(&observations.dir), None,
        ).is_some_and(|state| state.state == st_drivers::harness_state::Activity::Ended)
    {
        return Ok(false);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !root.join("provider-return.json").exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "fixture provider completion timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let deferred = observations.drain_live(client, subject, driver, ready).await?;
    fs::write(root.join("observation-drained"), if deferred { "deferred" } else { "published" })?;
    let after = fs::read_to_string(root.join("order"))? == "after";
    if after {
        while !task.is_finished() {
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "fixture JoinHandle completion timed out");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    } else {
        anyhow::ensure!(!task.is_finished(), "fixture must retain pending provider JoinHandle");
    }
    fs::write(root.join("join-phase"), if after { "finished" } else { "pending" })?;
    while !root.join("poll-driver").exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "fixture driver release timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(deferred)
}

async fn publish_harness_timeline(
    client: &Client,
    subject: &str,
    driver: &str,
    incarnation: &str,
    provider_incarnation: Option<&str>,
    agent_dir: &Path,
    published: &mut BTreeSet<String>,
) -> Result<()> {
    let Some(record) =
        st_drivers::harness_timeline::read(&st_drivers::harness_timeline::timeline_path(agent_dir))
    else {
        return Ok(());
    };
    // A replaced harness can leave a valid predecessor record at this stable path. It is history,
    // not authority for the live runtime, and must never be relabelled as the successor.
    if !timeline_record_is_current(&record, driver, provider_incarnation) {
        return Ok(());
    }
    for operation in record.operations {
        let publication = format!(
            "{}:{}:{}:{}",
            operation.incarnation_id, operation.entry_id, operation.revision, operation.operation
        );
        if published.contains(&publication) {
            continue;
        }
        // Usage ownership is graph state, not a driver fact. Persist no placeholder/null owner
        // fields here; the client projection joins the exact desired owner run/generation/step at
        // its snapshot index and overwrites attribution on every explicit usage entry.
        let claim = native_timeline_claim(client, subject, incarnation, operation).await?;
        let _: ClaimRecord = ObservationClient { client, event: None }
            .post(
                "/v1/claims",
                &claim,
            )
            .await?;
        published.insert(publication);
    }
    // The producer is bounded to the same order of magnitude. Forget publications no longer in
    // its record so this in-memory acceleration is bounded too; durable API idempotency remains
    // the restart/replay authority.
    if published.len() > 8_192 {
        published.clear();
    }
    Ok(())
}

/// The driver cannot relaunch the native session its suspended seat names. Record the typed
/// reason where the daemon's resume reads it, and end this launch: a harness must never start on
/// another session in its place.
async fn refuse_native_resume(
    client: &Client,
    subject: &str,
    incarnation: &str,
    driver: &str,
    refusal: st3::native_resume::Refusal,
) -> anyhow::Error {
    let _ = write_driver_log(
        subject,
        &json!({"type":"native_resume_refused","driver":driver,"code":refusal.code,"reason":refusal.reason}).to_string(),
    );
    let diagnostic = ClaimInput {
        subject: subject.into(),
        kind: "harness.diagnostic".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("severity".into(), Value::String("error".into())),
            ("status".into(), Value::String(refusal.code.into())),
            (
                "code".into(),
                Value::String(st3::suspension::RESUME_UNAVAILABLE_CODE.into()),
            ),
            ("reason".into(), Value::String(refusal.reason.clone())),
            ("incarnation_id".into(), Value::String(incarnation.into())),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!(
            "{}:{subject}:{incarnation}",
            st3::suspension::RESUME_UNAVAILABLE_CODE
        )),
    };
    if let Err(error) = retry_while_daemon_unreachable(subject, || {
        client.post::<_, ClaimRecord>("/v1/claims", &diagnostic)
    })
    .await
    {
        let _ = write_driver_log(
            subject,
            &json!({"type":"native_resume_refusal_unrecorded","error":format!("{error:#}")}).to_string(),
        );
    }
    anyhow::anyhow!(
        "{driver} cannot resume its native session ({}): {}",
        refusal.code,
        refusal.reason
    )
}

/// The driver cannot continue the seat's last native session, so the harness starts a new one.
/// Record why once for that session, so later relaunches start anew without trying it again.
async fn skip_native_continue(
    client: &Client,
    subject: &str,
    incarnation: &str,
    driver: &str,
    session: &str,
    refusal: st3::native_resume::Refusal,
) {
    let _ = write_driver_log(
        subject,
        &json!({"type":"native_continue_skipped","driver":driver,"session":session,"code":refusal.code,"reason":refusal.reason}).to_string(),
    );
    let selector_scope = (refusal.code == "authored-session-selection")
        .then(|| std::env::var(st3::suspension::SELECTOR_SCOPE_ENV).ok())
        .flatten();
    let mut diagnostic = ClaimInput {
        subject: subject.into(),
        kind: "harness.diagnostic".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            ("severity".into(), Value::String("warning".into())),
            ("status".into(), Value::String(refusal.code.into())),
            (
                "code".into(),
                Value::String(st3::suspension::CONTINUE_UNAVAILABLE_CODE.into()),
            ),
            (
                "reason".into(),
                Value::String(format!(
                    "{driver} started a new session instead of continuing {session}: {}",
                    refusal.reason
                )),
            ),
            ("incarnation_id".into(), Value::String(incarnation.into())),
            ("session_id".into(), Value::String(session.into())),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(st3::suspension::typed_continue_unavailable_key(
            subject, session, refusal.code, selector_scope.as_deref(),
        )),
    };
    if let Some(scope) = selector_scope {
        diagnostic.fields.insert("selector_scope".into(), Value::String(scope));
    }
    if let Err(error) = retry_while_daemon_unreachable(subject, || {
        client.post::<_, ClaimRecord>("/v1/claims", &diagnostic)
    })
    .await
    {
        let _ = write_driver_log(
            subject,
            &json!({"type":"native_continue_skip_unrecorded","error":format!("{error:#}")}).to_string(),
        );
    }
}

/// Report the native session the harness bound for this incarnation, once per session.
async fn report_native_session(
    client: &Client,
    subject: &str,
    incarnation: &str,
    harness: &str,
    session: &str,
    path: Option<&Path>,
    reported: &mut Option<String>,
) -> Result<()> {
    if reported.as_deref() == Some(session) {
        return Ok(());
    }
    let _: ClaimRecord = client
        .post(
            "/v1/agents/native-session",
            &json!({
                "subject": subject,
                "actor": subject,
                "incarnation_id": incarnation,
                "harness": harness,
                "session_id": session,
                "path": path.map(|path| path.to_string_lossy().into_owned()),
                "account_ref": std::env::var("ST3_ACCOUNT").ok(),
                "desired_token": std::env::var(st3::suspension::LAUNCH_DESIRED_TOKEN_ENV)
                    .ok().filter(|token| !token.is_empty()),
            }),
        )
        .await?;
    *reported = Some(session.to_owned());
    Ok(())
}

/// The thread a Codex wrapper bound in `binding.json` since `prior` was read at launch.
fn codex_bound_thread(state_dir: &Path, prior: Option<&Vec<u8>>) -> Option<String> {
    let bytes = fs::read(state_dir.join("binding.json")).ok()?;
    if Some(&bytes) == prior {
        return None;
    }
    let binding: Value = serde_json::from_slice(&bytes).ok()?;
    binding["threadId"]
        .as_str()
        .filter(|thread| !thread.is_empty())
        .map(str::to_owned)
}

async fn publish_harness_state(
    client: &Client,
    subject: &str,
    driver: &str,
    state: &str,
    incarnation: Option<&str>,
    reason: Option<&str>,
) -> Result<()> {
    let mut fields = BTreeMap::from([
        ("state".into(), Value::String(state.into())),
        ("driver".into(), Value::String(driver.into())),
    ]);
    if let Some(incarnation) = incarnation {
        fields.insert("incarnation_id".into(), Value::String(incarnation.into()));
    }
    if let Some(reason) = reason {
        fields.insert("reason".into(), Value::String(reason.into()));
    }
    st3::suspension::annotate_quiescence(&mut fields);
    let incarnation_key = work_incarnation_key(incarnation);
    let _: ClaimRecord = client
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: subject.into(),
                kind: "harness.observed".into(),
                actor: Some(subject.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("harness-state:{subject}:{incarnation_key}:{state}")),
            },
        )
        .await?;
    Ok(())
}

fn activate_channel_todo_observations(
    catalog: &Path, subject: &str, state: &mut PiChannelResume,
) -> Result<NativeObservations> {
    let dir = prepare_channel_todo_outbox(catalog, subject, &state.incarnation)?;
    st_drivers::harness_events::enable(&dir, &state.incarnation)?;
    let observations = NativeObservations::start(&dir, &state.incarnation)?;
    state.todo_outbox = Some(dir);
    state.record_pending_todo()?;
    Ok(observations)
}

async fn run_pi_channel(
    client: &Client,
    subject: &str,
    driver: &str,
    catalog: &Path,
) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let identity = subject.strip_prefix("agent/").unwrap_or(subject);
    let resumed = match st_drivers::reexec::resume_path(st_drivers::reexec::CHANNEL_RESUME_ENV) {
        Some(path) => {
            let state = st_drivers::reexec::read_state::<PiChannelResume>(&path);
            st_drivers::reexec::unblock_stop_signals();
            Some(state.context("resuming the pi-family channel after a binary replacement")?)
        }
        None => None,
    };
    let mut stdout = cli_output::async_stdout();
    let mut state = match resumed {
        // The extension already has its hello; a second one would restate the session context.
        Some(state) => {
            let _ = write_driver_log(
                subject,
                &format!("the {driver} channel resumed after its st binary was replaced"),
            );
            state
        }
        None => {
            let incarnation = wait_for_agent_incarnation(client, subject).await?;
            if push_mailbox_enabled() {
                // An argv seat can run its channel before reconciliation publishes
                // runtime.running. Give bind the same starting handshake as a typed
                // harness; it still allocates no ownership until this incarnation is live.
                retry_while_daemon_unreachable(subject, || {
                    publish_harness_state(client, subject, driver, "starting", Some(&incarnation), None)
                }).await?;
            }
            let context_name = format!("doc/context/{identity}/now");
            let context = retry_while_daemon_unreachable(subject, || {
                latest_document_text(client, &context_name)
            })
            .await?
            .unwrap_or_default();
            let session_context = pi_family_session_context(identity, &context);
            stdout
                .write_all(
                    format!(
                        "{}\n",
                        serde_json::to_string(&json!({
                            "type": "hello",
                            "protocol": if push_mailbox_enabled() { 2 } else { 1 },
                            "identity": identity,
                            "sessionContext": session_context,
                        }))?
                    )
                    .as_bytes(),
                )
                .await?;
            stdout.flush().await?;
            PiChannelResume {
                incarnation,
                session: st_drivers::contracts::env(if driver == "omp" {
                    st_drivers::omp_session::CHANNEL_SESSION
                } else {
                    st_drivers::pi_session::CHANNEL_SESSION
                })
                .unwrap_or_else(|| "unknown".into()),
                ..PiChannelResume::default()
            }
        }
    };
    let incarnation = state.incarnation.clone();
    let session = state.session.clone();
    let observation_paths = st_drivers::driver_paths::Paths::from_environment(identity, &|name| {
        std::env::var(name).ok()
    })?;
    let mut observer = if let Some(paths) = observation_paths
        && st_drivers::harness_events::enabled(&paths.agent_dir)
    {
        let (seq_env, runtime_env, driver_name) = if driver == "omp" {
            (
                st_drivers::omp_session::CHANNEL_SEQ,
                st_drivers::omp_session::CHANNEL_RUNTIME_ID,
                "omp",
            )
        } else {
            (
                st_drivers::pi_session::CHANNEL_SEQ,
                st_drivers::pi_session::CHANNEL_RUNTIME_ID,
                "pi",
            )
        };
        let seq = st_drivers::contracts::env(seq_env)
            .context("managed channel has no ownership sequence")?
            .parse()?;
        let runtime = st_drivers::contracts::env(runtime_env)
            .context("managed channel has no provider runtime")?;
        Some(st_drivers::pi_channel::EventObserver::new(
            &paths.agent_dir,
            identity,
            driver_name,
            &session,
            seq,
            &runtime,
        )?.with_native_session(state.native_session.clone()))
    } else {
        None
    };
    let mut todo_observations = if driver == "omp" && observer.is_none()
        && retry_while_daemon_unreachable(subject, || current_agent_incarnation(client, subject))
            .await?.as_deref() == Some(&incarnation)
    {
        Some(activate_channel_todo_observations(catalog, subject, &mut state)?)
    } else {
        if driver == "omp" && observer.is_none() {
            let _ = write_driver_log(subject, "todo publishing suspended until the graph incarnation catches up");
        }
        state.todo_outbox = None;
        None
    };
    let transport = format!("{driver}-channel");
    if push_mailbox_enabled() && state.pending.fence.is_none() {
        state.pending.fence = Some(st3::mailbox::Fence::new(subject, &incarnation, "delivery"));
    }
    if let Some(fence) = &mut state.pending.fence { fence.bind(client).await?; }
    let mut subscription = state.pending.fence.as_ref().map(|fence| {
        let mut report: Value =
            serde_json::from_str(&native_delivery_report(&transport, None)).unwrap_or_default();
        report["ready"] = json!(state.first_idle_seen && state.failed_handoffs.is_empty());
        st3::mailbox::Subscription::start(client.clone(), fence.clone(), report)
    });
    let mut pushed_messages: Vec<MessageView> = Vec::new();
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let spawn_reader = |sender: tokio::sync::mpsc::UnboundedSender<st_drivers::reexec::StdinChunk>| {
        st_drivers::reexec::StdinReader::spawn(move |chunk| sender.send(chunk).is_ok())
    };
    let mut reader = Some(spawn_reader(input_tx.clone()));
    let mut watch = st_drivers::reexec::ReplacementWatch::for_current_process();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_warning = None;
    let mut work_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    work_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renewed_minute = None;
    let mut work_retry = st3::client::DriverRetry::default();
    let mut checked_todo_minute = None;
    let mut todo_end_seen = None;
    loop {
        if cli_output::closed() {
            return Ok(());
        }
        tokio::select! {
            wake = async { match todo_observations.as_mut() {
                Some(observations) => observations.recv().await,
                None => std::future::pending().await,
            }} => {
                if let Err(error) = wake {
                    warn_pi_channel(subject, &error, &mut last_warning);
                } else if let Some(observations) = todo_observations.as_mut()
                    && let Err(error) = observations.drain(client, subject, driver, &mut false).await
                {
                    warn_pi_channel(subject, &error, &mut last_warning);
                }
            }
            frame = async { match &mut subscription {
                Some(subscription) => subscription.receiver.recv().await,
                None => std::future::pending().await,
            }} => {
                match frame {
                    Some(st3::mailbox::Frame::Mailbox { messages }) => {
                        let active: BTreeSet<_> = messages.iter().map(|message| message.subject.clone()).collect();
                        state.delivered.retain(|message| active.contains(message));
                        state.failed_handoffs.retain(|message, _| active.contains(message));
                        state.retry_after_ms.retain(|message, _| active.contains(message));
                        state.failed_diagnostics.retain(|message| active.contains(message));
                        pushed_messages = messages;
                    },
                    Some(st3::mailbox::Frame::Replay { nonce }) => {
                        if let Some(subscription) = &subscription { subscription.acknowledge_replay(nonce); }
                    },
                    Some(st3::mailbox::Frame::Drain { operation }) => {
                        if let Some(subscription) = &subscription { subscription.acknowledge_drain(operation); }
                    },
                    Some(st3::mailbox::Frame::Seat { seat }) => {
                        let frame = json!({"type":"seat", "seat":seat});
                        stdout.write_all(format!("{}\n", serde_json::to_string(&frame)?).as_bytes()).await?;
                        stdout.flush().await?;
                    },
                    Some(st3::mailbox::Frame::Fenced { reason }) => {
                        if let Some(dir) = &state.todo_outbox {
                            if let Err(error) = fs::remove_dir_all(dir) {
                                warn_pi_channel(subject, &error.into(), &mut last_warning);
                            }
                        }
                        anyhow::bail!("{reason}");
                    },
                    None => return Ok(()),
                }
            }
            chunk = input_rx.recv() => {
                let mut publish = false;
                match chunk {
                    Some(st_drivers::reexec::StdinChunk::Bytes(bytes)) => {
                        state.lines.push(&bytes);
                        while let Some(line) = state.lines.next_line() {
                            match accept_managed_channel_frame(
                                &mut state, &mut observer, &line,
                                // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
                                subject,
                                // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
                            ) {
                                Ok(changed) => publish |= changed,
                                Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
                            }
                        }
                    }
                    // The extension ends the pipe when its session ends.
                    Some(st_drivers::reexec::StdinChunk::Eof) | None => {
                        if let Some(line) = state.lines.finish()
                            && accept_managed_channel_frame(
                                &mut state, &mut observer, &line,
                                // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
                                subject,
                                // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
                            )?
                        {
                            let _ = state
                                .pending
                                .publish(client, subject, driver, &incarnation, &session)
                                .await;
                        }
                        if let Some(observations) = todo_observations.as_mut() {
                            if let Err(error) = observations.drain(client, subject, driver, &mut false).await {
                                warn_pi_channel(subject, &error, &mut last_warning);
                            }
                            if let Err(error) = finish_channel_todo_outbox(
                                client, subject, &incarnation, &observations.dir, &mut todo_end_seen,
                            ).await {
                                warn_pi_channel(subject, &error, &mut last_warning);
                            }
                        }
                        return Ok(());
                    }
                    Some(st_drivers::reexec::StdinChunk::Failed(error)) => {
                        return Err(error).context("reading the pi-family channel input");
                    }
                }
                if publish
                    && let Err(error) = state
                        .pending
                        .publish(client, subject, driver, &incarnation, &session)
                        .await
                {
                    warn_pi_channel(subject, &error, &mut last_warning);
                }
            }
            _ = interval.tick() => {
                if let Some(observer) = observer.as_mut() {
                    if let Err(error) = observer.heartbeat() {
                        warn_pi_channel(subject, &error, &mut last_warning);
                    }
                }
                if let Some(observations) = todo_observations.as_mut()
                    && observations.retry_pending
                    && let Err(error) = observations.drain(client, subject, driver, &mut false).await
                {
                    warn_pi_channel(subject, &error, &mut last_warning);
                }
                if let Err(error) = state
                    .pending
                    .publish(client, subject, driver, &incarnation, &session)
                    .await
                {
                    warn_pi_channel(subject, &error, &mut last_warning);
                }
                for message in state.failed_diagnostics.clone() {
                    let result: Result<ClaimRecord> = client.post("/v1/claims", &ClaimInput {
                        subject: subject.into(),
                        kind: "harness.diagnostic".into(),
                        actor: Some(subject.into()),
                        fields: BTreeMap::from([
                            ("severity".into(), Value::String("error".into())),
                            ("status".into(), Value::String("failed".into())),
                            ("code".into(), Value::String("pi-handoff-failed".into())),
                            ("reason".into(), Value::String(format!("the {driver} channel could not hand off {message} after three attempts"))),
                            ("incarnation_id".into(), Value::String(incarnation.clone())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("pi-handoff-failed:{subject}:{incarnation}:{message}")),
                    }).await;
                    match result {
                        Ok(_) => { state.failed_diagnostics.remove(&message); },
                        Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
                    }
                }
                // A recipient can read or close a failed handoff through another native
                // path. Its authoritative receipt settles that retry and its health warning.
                for message in state.failed_handoffs.keys().cloned().collect::<Vec<_>>() {
                    let view = if subscription.is_some() {
                        pushed_messages.iter().find(|view| view.subject == message).cloned()
                    } else { read_message(client, &message).await.ok() };
                    if view.is_some_and(|view| matches!(view.status.as_str(), "delivered" | "read" | "closed")) {
                        state.failed_handoffs.remove(&message);
                        state.retry_after_ms.remove(&message);
                        state.failed_diagnostics.remove(&message);
                    }
                }
                // A push report renews over the subscription even before the first idle proof.
                // Legacy channels attach the same report to their first mailbox page.
                let mut report: Value = serde_json::from_str(&native_delivery_report(&transport, None))?;
                report["ready"] = json!(state.first_idle_seen && state.failed_handoffs.is_empty());
                if !state.first_idle_seen {
                    report["reason"] = json!("the channel has not received the provider's initial idle proof");
                } else if !state.failed_handoffs.is_empty() {
                    report["reason"] = json!("the provider rejected a native handoff; the channel keeps retrying");
                }
                let report = report.to_string();
                if let Some(subscription) = &subscription {
                    subscription.report(serde_json::from_str(&report)?);
                }
                let mut cursor = None;
                let mut pending_silent = Vec::new();
                loop {
                    let page = if subscription.is_some() {
                        MessagePage { items: pushed_messages.clone(), has_more: false, next_cursor: None, limit: pushed_messages.len() }
                    } else {
                    match message_page_reporting(
                        client,
                        Some(subject),
                        false,
                        cursor.as_deref(),
                        cursor.is_none().then_some(report.as_str()),
                    )
                    .await
                    {
                        Ok(page) => page,
                        Err(error) => { warn_pi_channel(subject, &error, &mut last_warning); break; }
                    }
                    };
                    if !state.first_idle_seen {
                        break;
                    }
                    // A prior incarnation's handoff is not proof that the model consumed mail.
                    // The incarnation-local set survives channel reexec and prevents repeats here.
                    let mut items = page.items;
                    st3::silent::release_page(&mut items, &mut pending_silent);
                    for message in items.into_iter().filter(|message| matches!(message.status.as_str(), "sent" | "staged" | "delivered")) {
                    if state.retry_after_ms.get(&message.subject).is_some_and(|after|
                        current_unix_ms().unwrap_or_default() < u128::from(*after)) {
                        continue;
                    }
                    if !state.delivered.insert(message.subject.clone()) {
                        continue;
                    }
                    let body = match message_content(client, &message).await {
                        Ok(body) => body,
                        Err(error) => {
                            state.delivered.remove(&message.subject);
                            warn_pi_channel(subject, &error, &mut last_warning);
                            continue;
                        }
                    };
                    if message.status == "sent" {
                        let staged = match &state.pending.fence {
                            Some(fence) => mailbox_receipt_claim(client, fence, &message.subject, "staged").await.map(|claim| claim.kind == "message.staged"),
                            None => stage_pi_family_message(client, &message.subject, subject, driver).await,
                        };
                        match staged {
                            Ok(true) => {},
                            Ok(false) => { state.delivered.remove(&message.subject); continue; },
                            Err(error) => {
                                state.delivered.remove(&message.subject);
                                warn_pi_channel(subject, &error, &mut last_warning);
                                continue;
                            }
                        }
                    }
                    // Files first: a message that names an image is delivered once the image is here.
                    let attachments = match st3::blobs::materialize_for_seat(client, subject, &catalog.join("attachments"), &message).await {
                        Ok(attachments) => attachments,
                        Err(error) => {
                            state.delivered.remove(&message.subject);
                            warn_pi_channel(subject, &error, &mut last_warning);
                            continue;
                        }
                    };
                    let frame = pi_family_message_frame(&message, &body, identity, &attachments);
                    stdout.write_all(serde_json::to_string(&frame)?.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                    }
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
                let ready = match watch.as_mut() {
                    Some(watch) => tokio::task::block_in_place(|| watch.ready()),
                    None => None,
                };
                if let Some(binary) = ready {
                    // Take every byte the reader already consumed and act on every complete
                    // frame; only a partial frame crosses the exec.
                    if let Some(reader) = reader.take() {
                        tokio::task::block_in_place(|| reader.stop());
                    }
                    let mut publish = false;
                    while let Ok(chunk) = input_rx.try_recv() {
                        match chunk {
                            st_drivers::reexec::StdinChunk::Bytes(bytes) => state.lines.push(&bytes),
                            st_drivers::reexec::StdinChunk::Eof => return Ok(()),
                            st_drivers::reexec::StdinChunk::Failed(error) => {
                                return Err(error).context("reading the pi-family channel input");
                            }
                        }
                    }
                    while let Some(line) = state.lines.next_line() {
                        publish |= accept_managed_channel_frame(
                            &mut state, &mut observer, &line,
                            // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
                            subject,
                            // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
                        )?;
                    }
                    if publish {
                        let _ = state
                            .pending
                            .publish(client, subject, driver, &incarnation, &session)
                            .await;
                    }
                    stdout.flush().await?;
                    let state_root = if std::env::var_os(st_drivers::driver_paths::ROOT_ENV).is_some() { catalog } else { catalog.parent().unwrap_or(catalog) };
                    let _ = write_driver_log(
                        subject,
                        &format!(
                            "the st binary at {} was replaced; the {driver} channel re-executes into it",
                            binary.display()
                        ),
                    );
                    let failure = match st_drivers::reexec::write_state(state_root, "channel-resume", &state) {
                        Ok(path) => {
                            let error = st_drivers::reexec::exec(
                                &binary,
                                st_drivers::reexec::CHANNEL_RESUME_ENV,
                                &path,
                                &[],
                            );
                            let _ = fs::remove_file(&path);
                            format!("executing {} failed: {error}", binary.display())
                        }
                        Err(error) => format!("saving the channel resume state failed: {error:#}"),
                    };
                    let _ = write_driver_log(
                        subject,
                        &format!("the {driver} channel keeps its current binary: {failure}"),
                    );
                    if let Some(watch) = watch.as_mut() {
                        watch.refuse_current();
                    }
                    reader = Some(spawn_reader(input_tx.clone()));
                }
            }
            _ = work_retry.tick(&mut work_interval) => {
                let minute = unix_minute()?;
                if checked_todo_minute != Some(minute) {
                    checked_todo_minute = Some(minute);
                    if driver == "omp" && observer.is_none() && todo_observations.is_none() {
                        match current_agent_incarnation(client, subject).await {
                            Ok(current) if current.as_deref() == Some(&incarnation) => {
                                match activate_channel_todo_observations(catalog, subject, &mut state) {
                                    Ok(observations) => todo_observations = Some(observations),
                                    Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
                                }
                            }
                            Ok(_) => {}
                            Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
                        }
                    }
                    if let Err(error) = state.record_pending_todo() {
                        warn_pi_channel(subject, &error, &mut last_warning);
                    }
                    if let Some(observations) = todo_observations.as_ref() {
                        match remove_confirmed_ended_channel_todo_outbox(
                            client, subject, &incarnation, &observations.dir, &mut todo_end_seen,
                        ).await {
                            Ok(true) => return Ok(()),
                            Ok(false) => {}
                            Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
                        }
                    }
                }
                if let Some(renew_minute) = renewal_minute(subject)?
                    && renewed_minute != Some(renew_minute)
                    && work_retry.ready()
                {
                    match renew_claimed_work(client, subject, renew_minute, &mut work_retry).await {
                        Ok(()) => {
                            work_retry.succeeded();
                            renewed_minute = Some(renew_minute);
                        }
                        Err(error) => {
                            work_retry.failed(&error);
                            warn_pi_channel(subject, &error, &mut last_warning);
                        }
                    }
                }
            }
        }
    }
}

/// Note a failed pi-family channel request and keep the channel running. The channel shares a
/// terminal with its provider, so the note goes to the driver log, never to stderr.
fn warn_pi_channel(
    subject: &str,
    error: &anyhow::Error,
    last_warning: &mut Option<std::time::Instant>,
) {
    let now = std::time::Instant::now();
    if last_warning.is_none_or(|last| now.duration_since(last) >= Duration::from_secs(10)) {
        let line = match st3::client::daemon_unreachable(error) {
            Some(outage) => format!(
                "{}; the channel keeps running and retries every second until the daemon is back",
                outage.summary()
            ),
            None => format!("`{subject}` pi-family channel request failed; retrying: {error:#}"),
        };
        let _ = write_driver_log(subject, &line);
        *last_warning = Some(now);
    }
}

async fn latest_document_text(client: &Client, name: &str) -> Result<Option<String>> {
    let versions: DocumentListResponse = client
        .get(&format!("/v1/documents?name={}", urlencoding::encode(name)))
        .await?;
    let Some(version) = versions.items.into_iter().find(|version| version.latest) else {
        return Ok(None);
    };
    Ok(Some(String::from_utf8(
        document_bytes(client, &version.name, &version.hash).await?,
    )?))
}

/// Idle providers need no permit. Read graph control on mailbox arrival, then renew the
/// existing short lease only while mail waits; a late provider handoff still fails closed.
async fn sync_native_delivery_control(
    client: &Client,
    subject: &str,
    paths: &mut NativePaths,
    mailbox: &NativeMailbox,
    inbox: &Path,
    transport: &str,
) -> Result<()> {
    let report = |gate: &st_drivers::session_control::DeliveryGate| {
        if let Some(subscription) = &mailbox.subscription {
            subscription.report(native_delivery_control_report(transport, gate));
        }
    };
    let waiting = if mailbox.subscription.is_some() {
        mailbox.messages.iter().any(|message| {
            matches!(message.status.as_str(), "sent" | "staged" | "delivered")
        })
    } else {
        match st_drivers::message::list_dir(inbox) {
            Ok(messages) => !messages.is_empty(),
            Err(error) => {
                paths.delivery_gate.unavailable();
                report(&paths.delivery_gate);
                return Err(error);
            }
        }
    };
    if !waiting && paths.pending_hold_adoption.is_none() {
        paths.delivery_gate.idle();
        report(&paths.delivery_gate);
        return Ok(());
    }
    refresh_native_delivery_control(client, subject, paths, report).await
}

/// The only transitional status read: one fresh DND on an adopted predecessor, with no writes.
fn legacy_delivery_hold(
    subject: &str,
    agent_dir: &Path,
) -> Option<st3::delivery_hold::HoldRequest> {
    let until_unix_ms = st_drivers::status::dnd_deadline_ms(&agent_dir.join("status"))?;
    Some(st3::delivery_hold::HoldRequest {
        subject: subject.into(),
        actor: subject.into(),
        held: true,
        until_unix_ms,
        reason: "Unexpired delivery hold adopted from the previous native driver".into(),
        idempotency_key: format!("delivery-hold-adoption:{subject}:{until_unix_ms}"),
        legacy_adoption: true,
    })
}

async fn run_codex_native(client: &Client, subject: &str, argv: Vec<String>) -> Result<()> {
    anyhow::ensure!(!argv.is_empty(), "the Codex driver argv is empty");
    let incarnation = wait_for_agent_incarnation(client, subject).await?;
    // The local PTY can publish before reconciliation records runtime.running. Like the
    // other native drivers, publish startup evidence before binding the mailbox: that bind
    // must wait for the exact running incarnation instead of treating this fresh seat as stale.
    retry_while_daemon_unreachable(subject, || {
        publish_harness_state(
            client,
            subject,
            "codex",
            "starting",
            Some(&incarnation),
            None,
        )
    })
    .await?;
    if let Some(thread) = st3::native_resume::requested()
        && let Err(refusal) = st3::native_resume::codex_check(&argv, &thread)
    {
        return Err(refuse_native_resume(client, subject, &incarnation, "codex", refusal).await);
    }
    drive_codex_native(
        client,
        subject,
        argv,
        incarnation,
        ProviderStart::Launch(
            Vec::new(),
            // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
            Vec::new(),
            // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
        ),
        NativeLoopState::default(),
    )
    .await
}

/// The Codex thread a relaunch continues, unless the seat declares its own thread selection.
fn codex_continued_thread(argv: &[String]) -> Option<String> {
    st3::native_resume::continued()
        .map(|(thread, _)| thread)
        .filter(|thread| st3::native_resume::codex_check(argv, thread).is_ok())
}

/// How the Codex driver learns its person's answer to the approval it waits on: this daemon's
/// prompt state for the seat, read from the driver's blocking control thread.
fn codex_prompt_answers(
    client: &Client,
    subject: &str,
) -> st_drivers::session_control::PromptAnswers {
    let client = client.clone();
    let runtime = tokio::runtime::Handle::current();
    let agent = urlencoding::encode(subject).into_owned();
    let publisher = (client.clone(), runtime.clone(), subject.to_owned());
    st_drivers::session_control::PromptAnswers::new(move |ownership, transition| {
        let path = format!(
            "/v1/harness-prompts/state?agent={agent}&ownership={ownership}&transition={transition}"
        );
        runtime.block_on(async {
            // Bounded: the control thread reads Codex's socket between these reads.
            tokio::time::timeout(
                Duration::from_millis(500),
                client.get::<st_drivers::session_control::PromptAnswer>(&path),
            )
            .await
            .ok()?
            .ok()
        })
    })
    // Each approval's item, so a client offers to allow only that exact call.
    .with_call_publisher(move |ownership, transition, call| {
        let (client, runtime, subject) = &publisher;
        runtime.block_on(st3::driver_hook::publish_prompt_call(
            client, "codex", subject, ownership, transition, call,
        ));
    })
}

fn spawn_codex_provider(
    client: &Client,
    subject: &str,
    paths: &NativePaths,
    state_dir: &Path,
    argv: &[String],
    start: ProviderStart,
) -> tokio::task::JoinHandle<Result<()>> {
    let prompt_answers = codex_prompt_answers(client, subject);
    let paths = paths.clone();
    let state_dir = state_dir.to_path_buf();
    let argv = argv.to_vec();
    tokio::task::spawn_blocking(move || {
        let outcome = match start {
            // A resumed seat's launch environment names the thread it suspended on, and any other
            // relaunch the thread it continues.
            ProviderStart::Launch(
                _,
                // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
                _,
                // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
            ) => {
                let thread = st3::native_resume::requested().or_else(|| codex_continued_thread(&argv));
                st_drivers::codex_app_server::run_controlled_paths(
                    &paths.driver_root,
                    &state_dir,
                    &paths.agent_dir,
                    paths.identity,
                    paths.runtime_id,
                    argv,
                    paths.delivery_gate,
                    Some(prompt_answers),
                    thread,
                )
            }
            ProviderStart::Adopt(st_drivers::provider_session::DetachedSession::Codex {
                tui_pid,
                server_pid,
                watchdog_pid,
                owner_write_fd,
                socket_path,
                safe_fallback,
            }) => st_drivers::codex_app_server::adopt_controlled_paths(
                &paths.driver_root,
                &state_dir,
                &paths.agent_dir,
                paths.identity,
                paths.runtime_id,
                argv,
                tui_pid,
                server_pid,
                watchdog_pid,
                owner_write_fd,
                socket_path,
                safe_fallback,
                paths.delivery_gate,
                Some(prompt_answers),
            ),
            ProviderStart::Adopt(session) => {
                anyhow::bail!("a Codex driver cannot adopt this provider session: {session:?}")
            }
        };
        #[cfg(feature = "test-support")]
        if env!("CARGO_BIN_NAME") == "st3-fixture" {
            st3::test_support::hold_provider_completion(&outcome)?;
        }
        outcome
    })
}

async fn drive_codex_native(
    client: &Client,
    subject: &str,
    argv: Vec<String>,
    incarnation: String,
    start: ProviderStart,
    mut loop_state: NativeLoopState,
) -> Result<()> {
    let settle = loop_state.settle;
    let mut paths = if matches!(start, ProviderStart::Launch(
        _,
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
        _,
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
    )) {
        NativePaths::prepare(subject, "codex")?
    } else {
        NativePaths::resumed(subject, "codex", loop_state.paths.clone())?
    };
    if matches!(start, ProviderStart::Launch(
        _,
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
        _,
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
    )) {
        st_drivers::harness_events::enable(&paths.agent_dir, &incarnation)?;
    }
    loop_state.paths = Some(paths.resolved());
    let NativePaths {
        agent_dir,
        identity,
        runtime_id,
        ..
    } = paths.clone();
    let root = paths.state_root();
    let state_dir = paths.session_dir.clone();
    let harness_state_path = st_drivers::harness_state::harness_state_path(&agent_dir);
    if matches!(start, ProviderStart::Launch(
        _,
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
        _,
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
    )) {
        // The path can still hold the predecessor's terminal record. It is the predecessor's, never
        // this incarnation's: only a byte change after this point is the new wrapper's claim.
        loop_state.predecessor_harness_record = fs::read(&harness_state_path).ok();
        loop_state.harness_record_started = false;
    } else {
        loop_state.harness_record_started = true;
    }
    let prior_binding = std::fs::read(state_dir.join("binding.json")).ok();
    let inbox = st_drivers::message::inbox_dir(&agent_dir);
    let archive = st_drivers::message::archive_dir(&agent_dir);
    let mut mailbox =
        NativeMailbox::start(client, subject, &incarnation, "codex", &mut loop_state).await?;
    let mut observations = NativeObservations::start(&agent_dir, &incarnation)?.settle(settle);
    if push_mailbox_enabled() {
        st_drivers::push_mailbox::register(&agent_dir);
    }
    if matches!(start, ProviderStart::Adopt(_)) {
        paths.pending_hold_adoption = legacy_delivery_hold(subject, &paths.agent_dir);
    }
    // The thread this launch continues; a refusal of it ends the wrapper before it binds.
    let continued = matches!(start, ProviderStart::Launch(
        _,
        // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
        _,
        // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
    ))
        .then(|| codex_continued_thread(&argv))
        .flatten();
    let mut task = spawn_codex_provider(client, subject, &paths, &state_dir, &argv, start);
    let mut reported_session = None;
    // The Codex control pump keeps the subagent ledger; this driver records it on the seat.
    let mut subagents = st3::subagents::Publisher::start(
        subject,
        "codex",
        &incarnation,
        &agent_dir,
        st_drivers::subagents::now_ms(),
    );
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut work_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    work_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renewed_minute = None;
    let mut work_retry = st3::client::DriverRetry::default();
    let mut control_retry = st3::client::DriverRetry::default();
    let mut last_activity_fingerprint = None;
    let mut last_usage_fingerprint = None;
    let mut last_limits_fingerprint = None;
    let mut last_control_warning = None;
    let mut last_capacity_fingerprint = None;
    let mut delivery = NativeDeliverySupervisor::resumed(loop_state.delivery_episode);
    let mut replacement = DriverReplacement::new();
    // Defer only this provider's exact owned terminal observation until its task
    // returns; a foreign or superseded mailbox fence still ends the wrapper.
    let mut completion_announced = false;
    loop {
        tokio::select! {
            frame = mailbox.recv() => {
                #[cfg(feature = "test-support")]
                if env!("CARGO_BIN_NAME") == "st3-fixture"
                    && matches!(&frame, Some(st3::mailbox::Frame::Fenced { .. }))
                    && let Some(root) = std::env::var_os("ST3_FIXTURE_TERMINAL_COMPLETION")
                {
                    fs::write(PathBuf::from(root).join("fence-received"),
                        if task.is_finished() { "finished" } else { "pending" })?;
                }
                let mail_changed = matches!(&frame, Some(st3::mailbox::Frame::Mailbox { .. }));
                mailbox.accept(frame, &runtime_id)?;
                if !completion_announced && mail_changed
                    && let Err(error) = sync_native_delivery_control(client, subject, &mut paths, &mailbox, &inbox, "app-server").await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
            }

            wake = observations.recv(), if !completion_announced => {
                wake?;
                match observations.drain_live(client, subject, "codex", &mut loop_state.ready).await {
                    Ok(ended) => completion_announced |= ended,
                    Err(error) => note_driver_tick_failure(subject, error, &mut last_control_warning),
                }
                #[cfg(feature = "test-support")]
                if completion_announced { fixture_codex_completion_phase(&task).await?; }
            }
            result = &mut task, if completion_announced || fixture_completion_task_enabled() => {
                let outcome = result.context("joining the Codex driver")?;
                if let Some(session) = detached_session(&outcome) {
                    loop_state.delivery_episode = delivery.episode;
                    let resume = DriverResume {
                        driver: "codex".into(),
                        subject: subject.to_owned(),
                        incarnation: incarnation.clone(),
                        session: session.clone(),
                        loop_state,
                    };
                    let _ = replacement.exec(subject, &root, &resume);
                    loop_state = resume.loop_state;
                    task = spawn_codex_provider(client, subject, &paths, &state_dir, &argv, ProviderStart::Adopt(session));
                    completion_announced = false;
                    continue;
                }
                // A resume that ended before its thread bound was refused by Codex itself, such
                // as a thread with no rollout. The wrapper can end cleanly and leave the reason in
                // its harness record. The refusal goes first, so nothing after it can delay it.
                if reported_session.is_none() && st3::native_resume::requested().is_some() {
                    let reason = match &outcome {
                        Err(error) => format!("{error:#}"),
                        Ok(()) => st_drivers::harness_state::read(&harness_state_path, None)
                            .and_then(|observed| observed.reason)
                            .unwrap_or_else(|| "Codex ended before it bound the resumed thread".into()),
                    };
                    let refusal = st3::native_resume::Refusal {
                        code: "harness-refused",
                        reason: reason.chars().take(2_000).collect(),
                    };
                    let _ = refuse_native_resume(client, subject, &incarnation, "codex", refusal).await;
                } else if reported_session.is_none()
                    && let Some(thread) = &continued
                {
                    // The next relaunch starts a new thread instead of trying this one again.
                    let reason = match &outcome {
                        Err(error) => format!("{error:#}"),
                        Ok(()) => st_drivers::harness_state::read(&harness_state_path, None)
                            .and_then(|observed| observed.reason)
                            .unwrap_or_else(|| "Codex ended before it bound the continued thread".into()),
                    };
                    let refusal = st3::native_resume::Refusal {
                        code: "harness-refused",
                        reason: reason.chars().take(2_000).collect(),
                    };
                    skip_native_continue(client, subject, &incarnation, "codex", thread, refusal).await;
                }
                if let Err(error) = observations.drain(client, subject, "codex", &mut loop_state.ready).await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                // The harness is gone, and its subagents with it.
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    subagents.end_all(client, "harness-exited", "its harness exited"),
                )
                .await;
                if let Err(error) = &outcome {
                    let reason = format!("{error:#}").chars().take(2_000).collect::<String>();
                    let _: Result<ClaimRecord> = client.post("/v1/claims", &ClaimInput {
                        subject: subject.into(),
                        kind: "harness.diagnostic".into(),
                        actor: Some(subject.into()),
                        fields: BTreeMap::from([
                            ("severity".into(), Value::String("error".into())),
                            ("status".into(), Value::String("failed".into())),
                            ("code".into(), Value::String("codex-driver-failed".into())),
                            ("reason".into(), Value::String(reason)),
                            ("incarnation_id".into(), Value::String(incarnation.clone())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("codex-driver-failed:{subject}:{incarnation}")),
                    }).await;
                }
                return outcome;
            },
            settled = settle.control_tick(&mut control_retry, &mut interval), if !completion_announced => {
                if let Err(error) = observations.expire_due() {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }

                if observations.retry_due() {
                    match observations.drain_live(client, subject, "codex", &mut loop_state.ready).await {
                        Ok(ended) => completion_announced |= ended,
                        Err(error) => note_driver_tick_failure(subject, error, &mut last_control_warning),
                    }
                    if completion_announced {
                        #[cfg(feature = "test-support")]
                        fixture_codex_completion_phase(&task).await?;
                        continue;
                    }
                }

                if mailbox.subscription.is_some()
                    && let Err(error) = sync_native_delivery_control(client, subject, &mut paths, &mailbox, &inbox, "app-server").await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                // Delivery runs first and on its own: a failing observation publish must never
                // hold back a message.
                if mailbox.subscription.is_some() {
                    if let Err(error) = mailbox.pump(client, &agent_dir,
                        NativeDeliveryReceipts::Codex { state_dir: &state_dir, identity: &identity, runtime_id: &runtime_id }).await {
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                } else {
                delivery.report = Some(native_delivery_control_report("app-server", &paths.delivery_gate).to_string());
                supervise_native_delivery(
                    client,
                    subject,
                    &inbox,
                    &archive,
                    "app-server",
                    NativeDeliveryReceipts::Codex {
                        state_dir: &state_dir,
                        identity: &identity,
                        runtime_id: &runtime_id,
                    },
                    &incarnation,
                    &mut delivery,
                )
                .await;
                if let Err(error) = sync_native_delivery_control(client, subject, &mut paths, &mailbox, &inbox, "app-server").await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                }
                if settled && control_retry.ready() {
                let tick: Result<()> = async {
                    if !settled { return Ok(()) }
                    if !loop_state.ready && std::fs::read(state_dir.join("binding.json"))
                        .ok()
                        .is_some_and(|binding| Some(&binding) != prior_binding.as_ref())
                    {
                        let _: ClaimRecord = client.post("/v1/claims", &ClaimInput {
                            subject: subject.into(),
                            kind: "harness.observed".into(),
                            actor: Some(subject.into()),
                            fields: with_quiescence(BTreeMap::from([
                                ("state".into(), Value::String("ready".into())),
                                ("driver".into(), Value::String("codex".into())),
                                ("transport".into(), Value::String("app-server".into())),
                                ("incarnation_id".into(), Value::String(incarnation.clone())),
                            ])),
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: Some(format!("codex-ready:{subject}:{incarnation}")),
                        }).await?;
                        loop_state.ready = true;
                    }
                    if let Some(thread) = codex_bound_thread(&state_dir, prior_binding.as_ref())
                        && let Err(error) = report_native_session(
                            client,
                            subject,
                            &incarnation,
                            "codex",
                            &thread,
                            None,
                            &mut reported_session,
                        )
                        .await
                    {
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                    if observations.enabled { return Ok(()) }
                    let current_record = fs::read(&harness_state_path).ok();
                    loop_state.harness_record_started = harness_record_belongs_to_current_session(
                        loop_state.harness_record_started,
                        loop_state.predecessor_harness_record.as_deref(),
                        current_record.as_deref(),
                    );
                    if let Some(observed) = loop_state
                        .harness_record_started
                        .then(|| st_drivers::harness_state::read(&harness_state_path, None))
                        .flatten()
                    {
                        publish_harness_activity(
                            &ObservationClient { client, event: None },
                            subject,
                            "codex",
                            "app-server",
                            Some(&incarnation),
                            &observed,
                            &mut last_activity_fingerprint,
                        )
                        .await?;
                        let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
                            observed.since_ms,
                            observed.observed_at_ms,
                            observed.ownership_sequence,
                            observed.transition_sequence,
                            observed.reason.as_deref(),
                        ))?));
                        if observed.reason.as_deref() == Some("providerCapacity") {
                            if last_capacity_fingerprint.as_deref() != Some(fingerprint.as_str()) {
                                publish_provider_capacity_diagnostic(
                                    client,
                                    subject,
                                    &incarnation,
                                    observed.since_ms,
                                    &fingerprint,
                                )
                                .await?;
                                last_capacity_fingerprint = Some(fingerprint);
                            }
                        } else {
                            last_capacity_fingerprint = None;
                        }
                    }
                    if let Some(context) = st_drivers::harness_context::read(
                        &st_drivers::harness_context::harness_context_path(&agent_dir)) {
                    publish_harness_usage(
                        &ObservationClient { client, event: None },
                        subject,
                        "codex",
                        &incarnation,
                        &context,
                        &mut last_usage_fingerprint,
                    )
                    .await?;
                    publish_harness_limits(
                        &ObservationClient { client, event: None },
                        subject,
                        "codex",
                        &incarnation,
                        &context,
                        &mut last_limits_fingerprint,
                    )
                    .await?;
                    }
                    // The Codex runtime stamps its timeline with its own incarnation, which
                    // outlives st's runtime incarnation when a resident Codex is adopted.
                    let codex_incarnation = st_drivers::codex_app_server::current_runtime_incarnation(
                        &state_dir,
                        &identity,
                        &runtime_id,
                    );
                    publish_harness_timeline(
                        client,
                        subject,
                        "codex",
                        &incarnation,
                        codex_incarnation.as_deref(),
                        &agent_dir,
                        &mut loop_state.published_timeline,
                    )
                    .await?;
                    Ok(())
                }.await;
                match tick {
                    Ok(()) => control_retry.succeeded(),
                    Err(error) => {
                        control_retry.failed(&error);
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                }
                }
                subagents.set_timeline_incarnation(
                    st_drivers::codex_app_server::current_runtime_incarnation(
                        &state_dir,
                        &identity,
                        &runtime_id,
                    ),
                );
                if settled && let Err(error) = subagents.tick(client).await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                // Only a bound session can be adopted, so wait for the binding before following
                // a replacement.
                if loop_state.ready {
                    replacement.check();
                }
            }
            _ = settle.retry_tick(&mut work_retry, &mut work_interval), if !completion_announced && settle.passed() => {
                let tick: Result<()> = async {
                    if let Some(minute) = renewal_minute(subject)?
                        && renewed_minute != Some(minute)
                        && work_retry.ready()
                    {
                        renew_claimed_work(client, subject, minute, &mut work_retry).await?;
                        work_retry.succeeded();
                        renewed_minute = Some(minute);
                    }
                    Ok(())
                }.await;
                match tick {
                    Ok(()) => {}
                    Err(error) => {
                        work_retry.failed(&error);
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                }
            }
        }
    }
}

/// Called only after the actual live Codex drain deferred its matching terminal event.
/// This fixture controls JoinHandle ordering; it performs no drain, claim or fence itself.
#[cfg(feature = "test-support")]
async fn fixture_codex_completion_phase(task: &tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    if env!("CARGO_BIN_NAME") != "st3-fixture" { return Ok(()); }
    let Some(root) = std::env::var_os("ST3_FIXTURE_TERMINAL_COMPLETION").map(PathBuf::from) else {
        return Ok(());
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !root.join("provider-return.json").exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "Codex fixture provider completion timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    fs::write(root.join("observation-drained"), b"deferred")?;
    let after = fs::read_to_string(root.join("order"))? == "after";
    if after {
        while !task.is_finished() {
            anyhow::ensure!(tokio::time::Instant::now() < deadline, "Codex fixture JoinHandle completion timed out");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    } else {
        anyhow::ensure!(!task.is_finished(), "Codex fixture must retain pending provider JoinHandle");
    }
    fs::write(root.join("join-phase"), if after { "finished" } else { "pending" })?;
    while !root.join("poll-driver").exists() {
        anyhow::ensure!(tokio::time::Instant::now() < deadline, "Codex fixture driver release timed out");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

/// Once the provider is gone, retain unlimited exit-report retries across ordinary daemon
/// outages. A stop (including one arriving during a request) instead allows two seconds for
/// the final observation drain, subagent cleanup and terminal claim together. Polling the stop
/// flag takes at most 250 ms; timing out drops the in-flight request, not just the retry sleep.
async fn finish_native_exit_report(
    subject: &str,
    report: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    tokio::pin!(report);
    tokio::select! {
        result = &mut report => result,
        _ = async {
            while !st_drivers::provider_session::stop_requested() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        } => {
            match tokio::time::timeout(Duration::from_secs(2), report).await {
                Ok(result) => result,
                Err(_) => {
                    // The provider is already gone. The reconciler observes runtime liveness
                    // when the daemon returns; an unavailable API must not keep a stopped seat
                    // alive forever. Keep this note off the provider's shared terminal.
                    let _ = write_driver_log(subject,
                        "stopped driver reached its two-second exit-report deadline; exiting without an acknowledged terminal report");
                    Ok(())
                }
            }
        }
    }
}

/// Record a failed driver tick and keep going. A driver's control loop lives exactly as long as its
/// provider: an error it does not understand, such as a response shape a newer daemon changed, must
/// never end the loop while the provider keeps running, because nothing would deliver messages.
fn note_driver_tick_failure(
    subject: &str,
    error: anyhow::Error,
    last_warning: &mut Option<Instant>,
) {
    if let Err(error) = tolerate_driver_api_outage(subject, error, last_warning) {
        let now = Instant::now();
        if last_warning.is_none_or(|prior| now.duration_since(prior) >= Duration::from_secs(10)) {
            let _ = write_driver_log(
                subject,
                &format!("a driver tick failed; the driver keeps running and retries: {error:#}"),
            );
            *last_warning = Some(now);
        }
    }
}

/// Repeat one driver call until the daemon answers. A driver outlives daemon restarts, so an
/// outage while it starts delays the seat instead of ending it.
async fn retry_while_daemon_unreachable<T, F, Fut>(subject: &str, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_warning = None;
    let mut retry = st3::client::DriverRetry::default();
    loop {
        match call().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                let pause = retry.failed(&error);
                tolerate_driver_api_outage(subject, error, &mut last_warning)?;
                tokio::time::sleep(pause).await;
            }
        }
    }
}

fn work_incarnation_key(incarnation: Option<&str>) -> String {
    incarnation.map_or_else(
        || "unknown".into(),
        |value| hex::encode(Sha256::digest(value.as_bytes()))[..12].to_owned(),
    )
}

fn unix_minute() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 60)
}

fn renewal_minute(subject: &str) -> Result<Option<u64>> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    Ok(renewal_minute_at(subject, seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The idle-minute boot canary's property at its cause: an idle provider's driver tick reads
    /// no graph delivery control at all, and waiting mail reads it once per tick. Both mailbox
    /// sources (the push subscription agent seats use and the inbox directory) and both native
    /// transports are covered, so a regression is caught without a real idle minute.
    #[tokio::test]
    async fn native_delivery_control_reads_hold_only_while_mail_waits() {
        use axum::{Json, Router, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let subject = "agent/eval/gated";
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("control.sock");
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let app = Router::new().route("/v1/delivery/hold", get(move || {
            count.fetch_add(1, Ordering::SeqCst);
            async move {
                Json(json!({"api_version":"st3.v1", "value": {
                    "subject": subject, "active":false,
                    "until_unix_ms":null, "reason":null, "actor":null, "claim":null,
                }}))
            }
        }));
        let server_socket = socket.clone();
        let server = tokio::spawn(async move { serve_unix(&server_socket, app).await.unwrap() });
        for _ in 0..100 {
            if socket.exists() { break; }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(socket.exists());
        let client = Client::unix(&socket);

        for push in [true, false] {
            for transport in ["app-server", "opencode-server"] {
                let case = format!("push={push} transport={transport}");
                let inbox = root.path().join(format!("inbox-{push}-{transport}"));
                let mut paths = NativePaths {
                    delivery_gate: st_drivers::session_control::DeliveryGate::default(),
                    pending_hold_adoption: None,
                    driver_root: root.path().into(),
                    session_dir: root.path().into(),
                    agent_dir: root.path().into(),
                    identity: subject.into(),
                    runtime_id: "runtime-1".into(),
                };
                let fence = st3::mailbox::Fence::new(subject, "incarnation-1", "delivery");
                let mut mailbox = NativeMailbox {
                    // The stream never connects: the fake daemon serves only delivery control.
                    subscription: push.then(|| st3::mailbox::Subscription::start(
                        client.clone(), fence.clone(), json!({}),
                    )),
                    fence,
                    messages: Vec::new(),
                    queued: BTreeMap::new(),
                    replayed: true,
                    last_title_warning: None,
                };

                let before = reads.load(Ordering::SeqCst);
                for _ in 0..100 {
                    sync_native_delivery_control(&client, subject, &mut paths, &mailbox, &inbox, transport)
                        .await
                        .unwrap();
                }
                assert_eq!(reads.load(Ordering::SeqCst), before, "{case}: an idle tick read delivery control");
                assert!(paths.delivery_gate.held(), "{case}: idle must leave native input closed");

                if push {
                    mailbox.messages.push(serde_json::from_value(json!({
                        "subject": "message/waiting", "from": "person/eval", "to": subject,
                        "content": "Do the work.", "status": "sent", "created_index": 1,
                    })).unwrap());
                } else {
                    st_drivers::message::send_to_inbox(&inbox, "person/eval", None, None, &[], "Do the work.")
                        .unwrap();
                }
                let before = reads.load(Ordering::SeqCst);
                for tick in 1..=100 {
                    sync_native_delivery_control(&client, subject, &mut paths, &mailbox, &inbox, transport)
                        .await
                        .unwrap();
                    assert_eq!(reads.load(Ordering::SeqCst) - before, tick, "{case}: one read per waiting tick");
                }
                assert!(!paths.delivery_gate.held(), "{case}: an unheld seat's waiting mail opens native input");
            }
        }
        server.abort();
    }
}
