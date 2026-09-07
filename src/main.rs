//! st2 CLI. M0 exposes a single read-only command — `st2 ls <root>` — that slurps a catalog+inbox
//! folder and prints what it discovered (specs, warnings, errors). Reconcile/run land in later
//! milestones; this is the smoke test that discovery works end to end against a real folder.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};

use st2::{
    HostLock, Runner, SystemRunner, UpReport, detect_host, ding, discover, exec_state_dir, message,
    up_loop, up_once,
};

mod cli;

use cli::*;

fn main() -> Result<()> {
    let Cli {
        catalog_path,
        command,
    } = Cli::parse();

    // Claude's status-line tee is the one subcommand whose cadence a HARNESS sets rather than an
    // operator or an event: `refreshInterval: 5` makes it ~720 short-lived processes per hour per
    // seat, and Claude waits for each to exit. Building an OTel pipeline per render — and, at
    // exit, flushing it — would put a collector round-trip in the render path for a run that is
    // not an operation worth a span, so the tee never builds one (`DQ-C13`). This is a cadence
    // rule, not a hook rule: `claude-observe` is event-driven and stays instrumented, exactly as
    // `06-observability`'s spec names it.
    let mut telemetry = if matches!(command, Command::Driver(DriverCmd::ClaudeStatusline { .. })) {
        st2::telemetry::Telemetry::local_only()
    } else {
        st2::telemetry::Telemetry::init(if matches!(command, Command::Up { once: false, .. }) {
            "supervisor"
        } else if matches!(
            command,
            // Hook executions are their own process unit: `st2 driver claude-observe` runs per
            // Claude hook event and records hook_invocations_total, which the documented
            // process-unit contract assigns to `st2-hook`, not `st2-cli`.
            Command::Driver(DriverCmd::ClaudeObserve { .. })
        ) {
            "hook"
        } else {
            "cli"
        })
    };
    let result = dispatch(command, catalog_path.as_deref());
    telemetry.shutdown();
    result
}

fn dispatch(command: Command, catalog_path: Option<&std::path::Path>) -> Result<()> {
    initialize_catalog_env(catalog_path.map(|path| path.to_path_buf()).as_deref())?;
    match command {
        Command::Ls { root } => {
            let root = catalog_arg(root)?;
            ls(&root)
        }
        Command::Up {
            root,
            host,
            once,
            materialize_only,
            interval,
            agent,
            task,
        } => {
            let root = catalog_arg(root)?;
            if task.is_some() && !materialize_only && !once {
                anyhow::bail!("--task requires --once or --materialize-only");
            }
            if agent.is_some() && !materialize_only {
                anyhow::bail!("--agent requires --materialize-only");
            }
            up(&root, host, once, materialize_only, interval, agent, task)
        }
        Command::Message(cmd) => message_cmd(cmd),
        Command::Event(cmd) => event_cmd(cmd),
        Command::Stream(cmd) => stream_cmd(cmd),
        Command::Request(cmd) => request_cmd(cmd),
        Command::Context(cmd) => context_cmd(cmd),
        Command::Resource(cmd) => resource_cmd(cmd),
        Command::Service(cmd) => service_cmd(cmd),
        Command::ClaudeChannel(cmd) => claude_channel_cmd(cmd),
        Command::Hooks(cmd) => hooks_cmd(cmd),
        Command::Ding {
            session,
            identity,
            agent_id,
            root,
            host,
            interval,
        } => ding_cmd(session, identity, agent_id, root, host, interval),
        Command::CodexAppServer {
            identity,
            runtime_id,
            codex_argv,
        } => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::codex_app_server::run_controlled(&catalog, identity, runtime_id, codex_argv)
        }
        Command::ClaudeMcp { identity } => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::claude_mcp::run(&catalog, &identity)
        }
        Command::Driver(DriverCmd::Codex {
            identity,
            runtime_id,
            argv,
        }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::codex_app_server::run_controlled(&catalog, identity, runtime_id, argv)
        }
        Command::Driver(DriverCmd::PiChannel { identity }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::pi_channel::run(&catalog, &identity)
        }
        Command::Driver(DriverCmd::PiSession {
            identity,
            runtime_id,
            argv,
        }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::pi_session::run(&catalog, identity, runtime_id, argv)
        }
        Command::Driver(DriverCmd::ClaudeMcp { identity }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            let identity = identity
                .or_else(|| std::env::var("ST_AGENT").ok())
                .context("--identity is required when ST_AGENT is not set")?;
            st2::claude_mcp::run(&catalog, &identity)
        }
        Command::Driver(DriverCmd::OmpChannel { identity }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::pi_channel::run_omp(&catalog, &identity)
        }
        Command::Driver(DriverCmd::OmpSession {
            identity,
            runtime_id,
            argv,
        }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::omp_session::run(&catalog, identity, runtime_id, argv)
        }
        Command::Driver(DriverCmd::Claude { identity }) => {
            eprintln!("warning: `st2 driver claude` is deprecated; use `st2 driver claude-mcp`");
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::claude_mcp::run(&catalog, &identity)
        }
        Command::Driver(DriverCmd::ClaudeSession {
            identity,
            runtime_id,
            argv,
        }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::claude_session::run(&catalog, identity, runtime_id, argv)
        }
        Command::Driver(DriverCmd::ClaudeObserve {
            identity,
            runtime_id,
            event,
        }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::claude_session::run_observe(&catalog, &identity, runtime_id.as_deref(), &event)
        }
        Command::Driver(DriverCmd::ClaudeStatusline { identity }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::claude_session::run_statusline(&catalog, &identity)
        }
        Command::Driver(DriverCmd::OpencodeSession {
            identity,
            runtime_id,
            argv,
        }) => {
            let catalog = catalog_arg(None)?;
            let catalog = catalog.canonicalize().unwrap_or(catalog);
            st2::opencode_session::run(&catalog, identity, runtime_id, argv)
        }
        Command::Driver(DriverCmd::Expand { spec, agent, host }) => {
            let catalog = catalog_arg(None)?;
            driver_expand_cmd(&catalog, &spec, agent.as_deref(), host.as_deref())
        }
        Command::Status {
            identity,
            agent_id,
            set,
            ctx,
        } => status_cmd(identity, agent_id, set, ctx),
        Command::Rename(args) => presentation_cmd(st2::agent_author::PresentationField::Name, args),
        Command::Describe(args) => {
            presentation_cmd(st2::agent_author::PresentationField::Description, args)
        }
        Command::Agent(AgentCmd::Address(args)) => address_cmd(args),
        Command::Agent(AgentCmd::DesiredState {
            first,
            second,
            agent_id,
            reason,
            managed_by,
            host,
            json,
        }) => {
            let (identity, state) = match &agent_id {
                Some(_) => (None, first),
                None => (first, second),
            };
            let state = state.context(
                "a desired state is required: `running`, `suspended`, or `retired`",
            )?;
            anyhow::ensure!(
                matches!(state.as_str(), "running" | "suspended" | "retired"),
                "desired state must be `running`, `suspended`, or `retired`, not '{state}'"
            );
            desired_state_cmd(identity, agent_id, state, reason, managed_by, host, json)
        }
        Command::Agent(AgentCmd::Publish {
            spec,
            bundle,
            expect_absent,
            expect_sha256,
            input_sha256,
            managed_by,
            json,
        }) => {
            let catalog = catalog_arg(None)?;
            let source = match (spec, bundle) {
                (Some(path), None) => st2::agent_publish::PublishSource::Spec(path),
                (None, Some(path)) => st2::agent_publish::PublishSource::Bundle(path),
                _ => unreachable!("clap enforces one publication source"),
            };
            let expectation = match (expect_absent, expect_sha256) {
                (true, None) => st2::agent_publish::PublishExpectation::Absent,
                (false, Some(hash)) => st2::agent_publish::PublishExpectation::Sha256(hash),
                _ => unreachable!("clap enforces one publication expectation"),
            };
            let result = st2::agent_publish::publish(st2::agent_publish::PublishRequest {
                catalog,
                source,
                expectation,
                input_sha256,
                managed_by,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "{} {} {}",
                    match result.status {
                        st2::agent_publish::PublishStatus::Published => "published",
                        st2::agent_publish::PublishStatus::Unchanged => "unchanged",
                    },
                    result.bus_id,
                    result.path.display()
                );
            }
            Ok(())
        }
        Command::Agent(AgentCmd::Digest { spec, bundle, json }) => {
            let source = match (spec, bundle) {
                (Some(path), None) => st2::agent_publish::PublishSource::Spec(path),
                (None, Some(path)) => st2::agent_publish::PublishSource::Bundle(path),
                _ => unreachable!("clap enforces one source"),
            };
            let digest = st2::agent_publish::digest_source(source)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&digest)?);
            } else {
                println!("{}", digest.sha256);
            }
            Ok(())
        }
        Command::Catalog(CatalogCmd::Graph { host, json }) => {
            if !json {
                anyhow::bail!("`st2 catalog graph` v1 requires --json");
            }
            let graph = st2::catalog_graph::snapshot(
                &catalog_arg(None)?,
                &host.unwrap_or_else(detect_host),
            )?;
            let complete = graph.complete;
            println!("{}", serde_json::to_string_pretty(&graph)?);
            if !complete {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Catalog(CatalogCmd::Bootstrap {
            prepared,
            input_sha256,
            json,
        }) => {
            let result =
                st2::catalog_transaction::bootstrap(st2::catalog_transaction::BootstrapRequest {
                    catalog: catalog_arg(None)?,
                    prepared,
                    input_sha256,
                })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "{} {}",
                    match result.status {
                        st2::catalog_transaction::BootstrapStatus::Created => "created",
                        st2::catalog_transaction::BootstrapStatus::Unchanged => "unchanged",
                    },
                    result.root_sha256
                );
            }
            Ok(())
        }
        Command::Catalog(CatalogCmd::Digest { prepared, json }) => {
            let digest = st2::catalog_transaction::digest_prepared(&catalog_arg(None)?, &prepared)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&digest)?);
            } else {
                println!("{}", digest.root_sha256);
            }
            Ok(())
        }
        Command::Catalog(CatalogCmd::Diff {
            prepared,
            expect_sha256,
            json,
        }) => {
            if !json {
                anyhow::bail!("`st2 catalog diff` v1 requires --json");
            }
            let result = st2::catalog_transaction::diff(st2::catalog_transaction::DiffRequest {
                catalog: catalog_arg(None)?,
                prepared,
                expect_sha256,
            })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Command::Catalog(CatalogCmd::Snapshot {
            output,
            raw_preimage,
            json,
        }) => {
            let result =
                st2::catalog_transaction::snapshot(st2::catalog_transaction::SnapshotRequest {
                    catalog: catalog_arg(None)?,
                    output,
                    raw_preimage,
                })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "{} {} {}",
                    match result.status {
                        st2::catalog_transaction::SnapshotStatus::Created => "created",
                        st2::catalog_transaction::SnapshotStatus::Unchanged => "unchanged",
                    },
                    result.root_sha256,
                    result.output.display()
                );
            }
            Ok(())
        }
        Command::Catalog(CatalogCmd::Apply {
            prepared,
            input_sha256,
            expect_sha256,
            raw_preimage,
            resume,
            json,
        }) => {
            let mode = if resume {
                st2::catalog_transaction::ApplyMode::Resume
            } else {
                let prepared = prepared.context("clap requires --prepared unless --resume")?;
                let input_sha256 =
                    input_sha256.context("clap requires --input-sha256 unless --resume")?;
                let expect_sha256 =
                    expect_sha256.context("clap requires --expect-sha256 unless --resume")?;
                if raw_preimage {
                    st2::catalog_transaction::ApplyMode::RawPreimage {
                        prepared,
                        input_sha256,
                        expect_sha256,
                    }
                } else {
                    st2::catalog_transaction::ApplyMode::Prepared {
                        prepared,
                        input_sha256,
                        expect_sha256,
                    }
                }
            };
            let result = st2::catalog_transaction::apply(st2::catalog_transaction::ApplyRequest {
                catalog: catalog_arg(None)?,
                mode,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "{} {}",
                    match result.status {
                        st2::catalog_transaction::ApplyStatus::Applied => "applied",
                        st2::catalog_transaction::ApplyStatus::Unchanged => "unchanged",
                    },
                    result.after_sha256
                );
            }
            Ok(())
        }
        Command::Catalog(CatalogCmd::Archive {
            identity,
            all_retired,
            host,
            dry_run,
            json,
        }) => {
            let selection = if all_retired {
                st2::catalog_archive::Selection::AllRetired
            } else {
                st2::catalog_archive::Selection::Identities(identity)
            };
            let result = st2::catalog_archive::archive(st2::catalog_archive::ArchiveRequest {
                catalog: catalog_arg(None)?,
                host: host.unwrap_or_else(detect_host),
                selection,
                dry_run,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                let verb = if result.dry_run {
                    "would-archive"
                } else {
                    "archived"
                };
                for entry in &result.archived {
                    println!("{verb} {} {} -> {}", entry.id, entry.from, entry.to);
                }
                for refusal in &result.refused {
                    println!(
                        "skipped {} [{}] {}",
                        refusal.id, refusal.code, refusal.message
                    );
                }
            }
            Ok(())
        }
        Command::Catalog(CatalogCmd::Unarchive {
            identity,
            host,
            json,
        }) => {
            let result = st2::catalog_archive::unarchive(st2::catalog_archive::UnarchiveRequest {
                catalog: catalog_arg(None)?,
                host: host.unwrap_or_else(detect_host),
                identity,
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("unarchived {} {} -> {}", result.id, result.from, result.to);
            }
            Ok(())
        }
        Command::Agents {
            catalog,
            status,
            identity,
            agent_id,
            json,
            enrich,
            ctx,
        } => agents_cmd(catalog, status, identity, agent_id, json, enrich, ctx),
        Command::Tasks { host, json } => {
            if !json {
                anyhow::bail!("`st2 tasks` v1 requires --json");
            }
            let catalog = catalog_arg(None)?;
            tasks_cmd(&catalog, host)
        }
        Command::Unpark { task, host } => {
            let catalog = catalog_arg(None)?;
            unpark_cmd(&catalog, &task, host)
        }
        Command::Down { root, host } => {
            if root.is_none() && catalog_path.is_none() {
                anyhow::bail!(
                    "refusing implicit teardown target; pass --catalog <path> or an explicit catalog path"
                );
            }
            let root = catalog_arg(root)?;
            down_cmd(&root, host)
        }
        Command::Env { root } => {
            let root = catalog_arg(root)?;
            env_cmd(&root)
        }
        Command::Pretrust { dirs } => pretrust_cmd(&dirs),
        Command::Eval {
            folder,
            host,
            keep,
            json,
        } => eval_cmd(&folder, host, keep, json),
        Command::Validate {
            root,
            host,
            candidate,
            strict,
            json,
        } => {
            let root = catalog_arg(root)?;
            validate_cmd(&root, host, candidate, strict, json)
        }
        Command::Pty { args } => pty_cmd(&args),
        Command::Shell { args } => shell_cmd(&args),
        Command::Doctor {
            root,
            host,
            require_supervisor,
        } => {
            let root = catalog_arg(root)?;
            doctor_cmd(&root, host, require_supervisor)
        }
        Command::Completions { shell } => {
            // Generate from the live command tree so the script can never drift
            // from the actual flags (the flake gates this at build time).
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "st2", &mut std::io::stdout());
            Ok(())
        }
    }
}

fn driver_expand_cmd(
    catalog: &Path,
    path: &Path,
    agent: Option<&str>,
    host: Option<&str>,
) -> Result<()> {
    let (mut specs, warnings) = st2::discover_file(catalog, path)
        .with_context(|| format!("reading driver declaration {}", path.display()))?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    if let Some(agent) = agent {
        specs.retain(|spec| spec.identity == agent || spec.bus_id(host.unwrap_or("")) == agent);
    }
    anyhow::ensure!(
        specs.len() == 1,
        if agent.is_some() {
            format!(
                "{} contains {} matching agent blocks; expected exactly one",
                path.display(),
                specs.len()
            )
        } else {
            format!(
                "{} contains {} agent blocks; use --agent when it contains more than one",
                path.display(),
                specs.len()
            )
        }
    );
    let output = st2::driver::expand_driver(&specs[0], host.unwrap_or(""))?;
    print!("{output}");
    Ok(())
}

fn hooks_cmd(command: HooksCmd) -> Result<()> {
    match command {
        HooksCmd::Install {
            replace,
            allow_downgrade,
        } => {
            let dir = st2::hooks::install(replace || allow_downgrade)?;
            let root = st2::hooks::hooks_root()?;
            println!(
                "installed hook set {} in {}\nreceipt {}",
                st2::hooks::hookset_id(),
                dir.display(),
                root.join("current.json").display()
            );
        }
        HooksCmd::Verify => {
            let dir = st2::hooks::verify_installed()?;
            println!(
                "verified hook set {} in {}",
                st2::hooks::hookset_id(),
                dir.display()
            );
        }
        HooksCmd::VerifyOwn => {
            let dir = st2::hooks::verify_required_set()?;
            println!(
                "verified this binary's hook set {} in {}",
                st2::hooks::hookset_id(),
                dir.display()
            );
        }
    }
    Ok(())
}

fn down_cmd(root: &Path, host: Option<String>) -> Result<()> {
    // A single-file team spec: tear down the DECLARED team's sessions (symmetric with `st2 up`/`st2 ls`
    // over a spec — the "stop the fleet cleanly" verb). A catalog dir falls through to catalog teardown.
    if let Some(spec_file) = st2::eval_run::resolve_spec_path(root) {
        let (spec, spec_root) = st2::eval_run::load_spec(&spec_file)?;
        // Same host resolution as `st2 up <spec>`: --host › the spec's top-level host › OS hostname.
        let this_host = host
            .or_else(|| spec.host.clone())
            .unwrap_or_else(detect_host);
        let specs = st2::eval_run::spec_to_agent_specs(&spec.agents, &this_host, &spec_root);
        let runner = SystemRunner::new(spec_root, exec_state_dir(&this_host));
        let report = st2::down_specs(&specs, &this_host, &runner)?;
        println!(
            "teardown of spec {} on host '{this_host}':",
            spec_file.display()
        );
        print_report(&report);
        return Ok(());
    }

    let this_host = host.unwrap_or_else(detect_host);
    let catalog_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let runner = SystemRunner::new(catalog_root, exec_state_dir(&this_host));
    let report = st2::down(root, &this_host, &runner)?;
    println!("teardown on host '{this_host}':");
    print_report(&report);
    Ok(())
}

fn eval_cmd(folder: &Path, host: Option<String>, keep: bool, json: bool) -> Result<()> {
    let spec_file = st2::eval_run::resolve_spec_path(folder).with_context(|| {
        format!(
            "{} is not an st2 spec (a *.kdl file, or a folder with one)",
            folder.display()
        )
    })?;
    let keep = keep || std::env::var_os("ST2_EVAL_KEEP").is_some();
    if json {
        unsafe {
            std::env::set_var("ST2_EVAL_JSON", "1");
        }
    }
    let report = st2::eval_run::run_eval(&spec_file, host, keep)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        if report.passed() {
            return Ok(());
        }
        anyhow::bail!("VERDICT: FAIL")
    }

    if !report.done {
        println!(
            "(note: the team did not send a confirmation within max-timeout — judged the final state)"
        );
    }
    println!("\n== judges ==");
    let (mut pass, mut fail) = (0, 0);
    for j in &report.judges {
        if j.signal {
            // Show-but-don't-gate: runs + prints, but never counts toward SCORE/verdict.
            println!("  [SIGNAL] {}  ({})", j.name, j.detail);
            continue;
        }
        if j.passed {
            pass += 1;
        } else {
            fail += 1;
        }
        println!(
            "  {} {}  ({})",
            if j.passed { "[PASS]" } else { "[FAIL]" },
            j.name,
            j.detail
        );
    }
    println!(
        "SCORE: {pass} PASS / {fail} FAIL / {} gating judges",
        pass + fail
    );
    if report.passed() {
        println!("VERDICT: PASS");
        Ok(())
    } else {
        anyhow::bail!("VERDICT: FAIL")
    }
}

fn validate_cmd(
    root: &Path,
    host: Option<String>,
    candidate: Option<PathBuf>,
    strict: bool,
    json: bool,
) -> Result<()> {
    let catalog_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let host = host.unwrap_or_else(detect_host);
    let report = if let Some(candidate) = candidate {
        st2::agent_publish::validate_candidate_for_host(
            &catalog_root,
            st2::agent_publish::PublishSource::Spec(candidate),
            &host,
        )
    } else {
        let _catalog_lock = st2::CatalogLock::shared(&catalog_root)
            .context("acquire shared catalog-authoring lock for validation")?;
        st2::validate::validate_for_host(&catalog_root, &host)
    };
    let (errors, warnings) = (report.errors(), report.warnings());

    if json {
        let issues: Vec<serde_json::Value> = report
            .issues
            .iter()
            .map(|i| {
                serde_json::json!({
                    "severity": i.severity.tag(),
                    "code": i.code,
                    "path": i.path,
                    "agent": i.agent,
                    "message": i.message,
                })
            })
            .collect();
        let out = serde_json::json!({
            "schema": st2::validate::VALIDATE_RECEIPT_SCHEMA,
            "policyProfile": st2::validate::CORE_CATALOG_POLICY_PROFILE,
            "agentSpecRevision": agent_spec::AGENT_SPEC_REVISION,
            "issues": issues,
            "agents": report.agents,
            "errors": errors,
            "warnings": warnings,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        for i in &report.issues {
            println!("{}  {}: {}", i.severity.label(), i.path, i.message);
        }
        println!(
            "─ {errors} error{}, {warnings} warning{} across {} agent{}",
            plural(errors),
            plural(warnings),
            report.agents,
            plural(report.agents),
        );
    }

    // Exit non-zero on any error; under --strict, warnings fail too. Use a clean process exit so a
    // scriptable caller does not also get anyhow's "Error:" line after the report it already printed.
    if errors > 0 || (strict && warnings > 0) {
        std::process::exit(1);
    }
    Ok(())
}

/// The standard user catalog: `${XDG_STATE_HOME:-$HOME/.local/state}/st2/default/catalog`.
fn default_catalog_root() -> Option<PathBuf> {
    nonempty_env_path("XDG_STATE_HOME")
        .map(|state| state.join("st2/default/catalog"))
        .or_else(|| {
            nonempty_env_path("HOME").map(|home| home.join(".local/state/st2/default/catalog"))
        })
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Make even a not-yet-created output catalog absolute, so a later cwd change cannot retarget it.
fn absolute_catalog_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolving the current directory")?
            .join(path)
    };
    Ok(absolute.canonicalize().unwrap_or(absolute))
}

/// Seed `$CATALOG` once for every command. An explicit global flag wins over an inherited env var;
/// without either, the standard per-user catalog becomes the process default.
fn initialize_catalog_env(explicit: Option<&Path>) -> Result<()> {
    let selected = explicit
        .map(Path::to_path_buf)
        .or_else(|| nonempty_env_path("CATALOG"))
        .or_else(default_catalog_root);
    if let Some(path) = selected {
        let path = absolute_catalog_path(&path)?;
        // SAFETY: this is the first action after single-threaded CLI parsing, before any worker
        // threads or child processes exist.
        unsafe { std::env::set_var("CATALOG", path) };
    }
    Ok(())
}

/// Resolve an optional legacy positional path, otherwise use the shared catalog selection.
fn catalog_arg(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(path) => absolute_catalog_path(&path),
        None => catalog_root_for_env(),
    }
}

/// The selected catalog root for bus-aware commands: `$CATALOG`, then the standard user catalog.
/// `main` initializes `$CATALOG` from global `--catalog` before dispatch.
fn catalog_root_for_env() -> Result<PathBuf> {
    let root = nonempty_env_path("CATALOG")
        .or_else(default_catalog_root)
        .context(
            "no catalog selected: pass --catalog, set $CATALOG, or set $XDG_STATE_HOME/$HOME",
        )?;
    absolute_catalog_path(&root)
}

/// Set the same native catalog environment that `st2 env` prints. The catalog's own declared session
/// registry is used, not the caller's ambient one: these hand `pty` the roots of the *catalog*.
fn with_bus_env(cmd: &mut std::process::Command, root: &Path) {
    cmd.env("CATALOG", root)
        .env("ST_ROOT", root)
        .env("PTY_ROOT", st2::catalog::pty_root(root));
}

/// `st2 pty [<pty-args>…]` — a thin pass-through to `pty` with the catalog's bus env pre-set, so
/// the maintainer never has to `eval "$(st2 env …)"` first. **Replaces** this process with `pty` (via exec)
/// so the interactive UI keeps the tty, signals, and exit code.
fn pty_cmd(args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let root = catalog_root_for_env()?;
    let mut cmd = std::process::Command::new("pty");
    cmd.args(args);
    with_bus_env(&mut cmd, &root);
    // exec() only returns on failure (e.g. `pty` not on PATH).
    let err = cmd.exec();
    Err(anyhow::anyhow!("failed to exec `pty`: {err}"))
}

/// `st2 shell [<args>…]` — drop into `$SHELL` with the native catalog environment set. The general
/// form of `st2 pty`.
fn shell_cmd(args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let root = catalog_root_for_env()?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = std::process::Command::new(&shell);
    cmd.args(args);
    with_bus_env(&mut cmd, &root);
    let err = cmd.exec();
    Err(anyhow::anyhow!("failed to exec `{shell}`: {err}"))
}

fn env_cmd(root: &Path) -> Result<()> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let c = canonical.display();
    // The same roots st2 sets on every task it spawns.
    println!("export CATALOG={c}");
    println!("export ST_ROOT={c}");
    println!(
        "export PTY_ROOT={}",
        st2::catalog::pty_root(&canonical).display()
    );
    Ok(())
}

fn pretrust_cmd(dirs: &[PathBuf]) -> Result<()> {
    let n = st2::pretrust::pretrust(dirs)?;
    println!(
        "pre-trusted {n} workspace{} in the ambient Claude and Codex configs",
        if n == 1 { "" } else { "s" }
    );
    Ok(())
}

fn doctor_cmd(root: &Path, host: Option<String>, require_supervisor: bool) -> Result<()> {
    let this_host = host.unwrap_or_else(detect_host);
    let catalog = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    println!(
        "st2 doctor — catalog {}, host '{this_host}'",
        catalog.display()
    );
    let mut problems = 0usize;

    // 1) The tools a running fleet needs.
    report_check(
        &mut problems,
        tool_on_path("pty"),
        "`pty` on PATH",
        "not found",
    );

    // 2) Supervision mode. A one-shot/manual host intentionally has no lock. A caller that expects
    // a resident loop can opt into enforcing one; a stale file always indicates an unclean exit.
    let host_lock = st2::HostLock::new(root, &this_host);
    match host_lock.live_owner() {
        Some(_) => report_check(&mut problems, true, "supervisor (st2 up) running", ""),
        None if host_lock.has_stale_lock() => report_check(
            &mut problems,
            false,
            "supervision host-lock healthy",
            "stale host-lock from a dead supervisor",
        ),
        None if !require_supervisor => report_check(
            &mut problems,
            true,
            "supervision mode manual/--once (no live host-lock)",
            "",
        ),
        None => report_check(
            &mut problems,
            false,
            "supervisor (st2 up) running",
            "required but no live host-lock — run `st2 up`",
        ),
    }

    // 3) Per this-host declaration: active tasks require liveness and fresh presence; suspended
    // tasks require no live work; retired tasks require complete record absence.
    let _catalog_lock = st2::CatalogLock::shared(&catalog)
        .context("acquire shared catalog-authoring lock for doctor snapshot")?;
    let found = discover(&catalog);
    for e in &found.errors {
        report_check(
            &mut problems,
            false,
            &format!("catalog file {}", e.path.display()),
            &e.message,
        );
    }
    let runner = SystemRunner::new(catalog.clone(), exec_state_dir(&this_host));
    let sessions = match runner.list_sessions() {
        Ok(sessions) => sessions,
        Err(error) => {
            report_check(
                &mut problems,
                false,
                "task runtime readable",
                &format!("{error:#}"),
            );
            anyhow::bail!("{problems} problem(s) found");
        }
    };
    let live: std::collections::HashSet<String> = sessions
        .iter()
        .filter(|s| s.alive)
        .map(|s| s.pty_id.clone())
        .collect();
    let present: std::collections::HashMap<String, bool> = sessions
        .into_iter()
        .map(|session| (session.pty_id, session.alive))
        .collect();
    for spec in &found.specs {
        if spec.resolved_host(&this_host) != this_host {
            continue;
        }
        let bus_id = spec.bus_id(&this_host);
        if let Some(dir) = spec.path.parent() {
            match message::inspect_sent(dir, false) {
                Ok(_) => report_check(
                    &mut problems,
                    true,
                    &format!("{bus_id} outbound message ledger"),
                    "",
                ),
                Err(error) => report_check(
                    &mut problems,
                    false,
                    &format!("{bus_id} outbound message ledger"),
                    &format!("cannot send: {error:#}"),
                ),
            }
        }
        // DELTA-006's Resolution Signal, per seat. Silence means both clauses are clear here, so
        // the `delivery-state.json` boundary arm is removable once every admitted host is silent
        // for the record's window — a trigger nobody produces resolves on memory instead.
        // Advisory, not a problem: a carried-forward attempt is correct behaviour today.
        let delivery_state_dirs = [
            st2::codex_app_server::state_dir(&catalog, &bus_id),
            st2::opencode_session::state_dir(&catalog, &bus_id),
        ];
        match st2::migrations::delivery_state::resolution_signal(&delivery_state_dirs) {
            Ok(signal) if signal.is_clear() => {}
            Ok(signal) => report_advisory(
                &format!("{bus_id} pre-ledger delivery state (DELTA-006)"),
                &format!(
                    "preLedgerRecords={} assertedEntries={}",
                    signal.pre_ledger_records, signal.asserted_entries
                ),
            ),
            Err(error) => report_check(
                &mut problems,
                false,
                &format!("{bus_id} delivery ledger readable"),
                &format!("{error:#}"),
            ),
        }
        if spec.desired_state.is_retired() {
            let still_present = spec
                .tasks
                .iter()
                .map(|task| {
                    task.id
                        .clone()
                        .unwrap_or_else(|| format!("{bus_id}.{}", task.name))
                })
                .filter_map(|id| {
                    present
                        .get(&id)
                        .map(|alive| format!("{id} ({})", if *alive { "alive" } else { "dead" }))
                })
                .collect::<Vec<_>>();
            report_check(
                &mut problems,
                still_present.is_empty(),
                &format!("{bus_id} retirement complete (all declared tasks absent)"),
                &format!("still present: {}", still_present.join(", ")),
            );
            continue;
        }
        if spec.desired_state.is_suspended() {
            let not_converged = spec
                .tasks
                .iter()
                .filter_map(|task| {
                    let id = task
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("{bus_id}.{}", task.name));
                    present.get(&id).and_then(|alive| {
                        (*alive || !(task.keep || spec.keep)).then(|| {
                            format!("{id} ({})", if *alive { "alive" } else { "dead non-keep" })
                        })
                    })
                })
                .collect::<Vec<_>>();
            report_check(
                &mut problems,
                not_converged.is_empty(),
                &format!("{bus_id} suspension effective (no live tasks)"),
                &format!("still present: {}", not_converged.join(", ")),
            );
            continue;
        }
        if !spec.has_delivery_transport() {
            report_advisory(
                &format!("{bus_id} delivery transport missing"),
                "declare `ding`, `deliver`, or a driver block; agent receives no DING",
            );
        }
        for task in &spec.tasks {
            let id = task
                .id
                .clone()
                .unwrap_or_else(|| format!("{bus_id}.{}", task.name));
            report_check(
                &mut problems,
                live.contains(&id),
                &format!("{bus_id} task '{}' alive", task.name),
                "session dead/missing",
            );
        }
        if let Some(dir) = spec.path.parent() {
            let path = st2::status::status_path(dir);
            if !path.is_file() {
                report_check(
                    &mut problems,
                    false,
                    &format!("{bus_id} presence missing"),
                    "no status file — is its session owner refreshing presence?",
                );
            } else {
                let state = st2::status::read_state(&path);
                report_check(
                    &mut problems,
                    state != st2::status::State::Unknown,
                    &format!("{bus_id} presence fresh (is `{}`)", state.as_str()),
                    "rotted to `unknown` — is its session owner refreshing presence?",
                );
            }
            // Observed harness state is advisory-only in doctor: absence names a driver gap and
            // a derived `unknown` names its reason, but neither fails the exit code.
            let observed_path = st2::harness_state::harness_state_path(dir);
            let pty_root = st2::agents::probe_pty_root(&catalog);
            let probe = |session: &str| st2::ding::session_liveness_in(&pty_root, session);
            match st2::harness_state::read(&observed_path, Some(&probe)) {
                None => report_advisory(
                    &format!("{bus_id} observed harness state absent"),
                    "no driver has published a harness-state record for this agent",
                ),
                Some(observed) if observed.state == st2::harness_state::Activity::Unknown => {
                    report_advisory(
                        &format!("{bus_id} observed harness state indeterminate"),
                        &format!(
                            "derived `unknown` ({}) — is its driver still observing the harness?",
                            observed.reason.as_deref().unwrap_or("unstated")
                        ),
                    )
                }
                Some(observed)
                    if observed.state == st2::harness_state::Activity::Ended
                        && spec.desired_state.as_str() == "running" =>
                {
                    report_advisory(
                        &format!("{bus_id} observed harness state ended"),
                        &format!(
                            "session ended ({}) while desired state is running — crashed seat?",
                            observed
                                .exit
                                .as_deref()
                                .map(str::to_owned)
                                // A terminal record can carry only a reason — Codex's
                                // observed systemError writes reason without an exit — and
                                // discarding it leaves the operator nothing to act on.
                                .or_else(|| {
                                    observed.reason.as_deref().map(|reason| format!("{reason}"))
                                })
                                .unwrap_or_else(|| "exit unstated".to_string())
                        ),
                    )
                }
                Some(observed) => report_check(
                    &mut problems,
                    true,
                    &format!(
                        "{bus_id} observed harness state fresh (is `{}`)",
                        observed.state.as_str()
                    ),
                    "",
                ),
            }
            // Harness context is advisory-only too (HC-R17), and on a narrower axis than the
            // categorical record above: a filling window is worth a human's attention, and it is
            // never a health failure. Absence says nothing here — no producer has published a
            // number, which is not a fault — so only a high reading and a stale record beside a
            // `running` desired state speak.
            if let Some(context) =
                st2::harness_context::read(&st2::harness_context::harness_context_path(dir))
            {
                // The harness's own percent, never one st2 divided out of the operands beside it,
                // and never clamped: an overrun above 100 is exactly what this warns about.
                if context.used_percent.is_some_and(|percent| {
                    percent >= st2::harness_context::HARNESS_CONTEXT_WARN_PERCENT
                }) {
                    report_advisory(
                        &format!(
                            "{bus_id} harness context at {}%",
                            context.used_percent.unwrap_or_default().round()
                        ),
                        &format!(
                            "at or above st2's {}% attention threshold, read {} ago — st2's own \
                             number, not a prediction of where this harness compacts",
                            st2::harness_context::HARNESS_CONTEXT_WARN_PERCENT.round(),
                            humanize_ms(context.age_ms)
                        ),
                    );
                }
                if context.stale && spec.desired_state.as_str() == "running" {
                    report_advisory(
                        &format!("{bus_id} harness context stale"),
                        &format!(
                            "the numbers are {} old while desired state is running — is its \
                             driver still reading the harness?",
                            humanize_ms(context.age_ms)
                        ),
                    );
                }
            }
            if st2::driver_diagnostic::expected_for(spec) {
                let diagnostic = st2::driver_diagnostic::read(&st2::driver_diagnostic::path(dir));
                match &diagnostic {
                    st2::driver_diagnostic::Observed::Failure(failure) => report_advisory(
                        &format!(
                            "{bus_id} native driver diagnostic: {}/{}",
                            failure.stage.as_str(),
                            failure.reason.as_str()
                        ),
                        st2::driver_diagnostic::repair_text(&diagnostic),
                    ),
                    st2::driver_diagnostic::Observed::Indeterminate(reason) => report_advisory(
                        &format!(
                            "{bus_id} native driver diagnostic indeterminate ({})",
                            reason.as_str()
                        ),
                        st2::driver_diagnostic::repair_text(&diagnostic),
                    ),
                    // Absence is only a fault where the driver publishes a boundary result on
                    // every launch; for Claude and Codex it is the healthy steady state.
                    st2::driver_diagnostic::Observed::Absent => {
                        if st2::driver_diagnostic::absence_is_a_fault(spec) {
                            report_advisory(
                                &format!("{bus_id} native driver diagnostic absent"),
                                st2::driver_diagnostic::repair_text(&diagnostic),
                            );
                        }
                    }
                }
            }
        }
    }

    if problems == 0 {
        println!("\n✓ all checks passed");
        Ok(())
    } else {
        anyhow::bail!("{problems} problem(s) found")
    }
}

fn tasks_cmd(root: &Path, host: Option<String>) -> Result<()> {
    let host = host.unwrap_or_else(detect_host);
    let catalog = match root.canonicalize() {
        Ok(catalog) => catalog,
        Err(error) => {
            let detail = format!("canonicalize catalog {}: {error}", root.display());
            let inventory =
                st2::task_inventory::TaskInventory::incomplete(root.to_path_buf(), host, detail);
            println!("{}", inventory.to_json());
            anyhow::bail!("task inventory incomplete")
        }
    };
    let before = match st2::catalog_lock::read_fence(&catalog) {
        Ok(fence) => fence,
        Err(error) => return print_incomplete_tasks(catalog, host, error.to_string()),
    };
    let found = discover(&catalog);
    let observed = match st2::catalog_lock::read_fence(&catalog) {
        Ok(fence) if fence == before => fence,
        Ok(_) => {
            return print_incomplete_tasks(
                catalog,
                host,
                "catalog generation changed during task discovery".to_string(),
            );
        }
        Err(error) => return print_incomplete_tasks(catalog, host, error.to_string()),
    };
    let runner = SystemRunner::new(catalog.clone(), exec_state_dir(&host));
    let parks = match st2::park::DirParkObserver::for_supervisor(&catalog, &host) {
        Ok(parks) => parks,
        Err(error) => {
            return print_incomplete_tasks(
                catalog,
                host,
                format!("open supervisor park projection: {error}"),
            );
        }
    };
    let mut inventory = st2::task_inventory::inventory(&catalog, &host, &found, &runner, &parks);
    let after = discover(&catalog);
    if !st2::task_inventory::same_discovery(&found, &after) {
        inventory.mark_incomplete("catalog declarations changed during task observation");
    }
    match st2::catalog_lock::read_fence(&catalog) {
        Ok(after) if after == observed => {}
        Ok(_) => inventory.mark_incomplete("catalog generation changed during task observation"),
        Err(error) => inventory.mark_incomplete(error.to_string()),
    }
    println!("{}", inventory.to_json());
    if inventory.complete() {
        Ok(())
    } else {
        anyhow::bail!("task inventory incomplete")
    }
}

/// Ask this host's supervisor to release one parked task.
///
/// The request is a file the supervisor drains at the top of its next pass, not a direct mutation:
/// the parked set lives in the supervisor's memory, and it is the only writer. That also means this
/// is best-effort by construction — with no supervisor running there is nothing to grant it, which is
/// why the confirmation says "requested" rather than claiming the task is back.
fn unpark_cmd(catalog: &Path, task: &str, host: Option<String>) -> Result<()> {
    let host = host.unwrap_or_else(detect_host);
    let catalog = catalog
        .canonicalize()
        .with_context(|| format!("canonicalize catalog {}", catalog.display()))?;
    let dir = st2::park::SupervisorScope::current(&catalog, &host)?.unpark_request_dir();
    st2::park::request_unpark(&dir, task)?;
    println!(
        "unpark requested for '{task}' in catalog {} on {host}; that supervisor grants it on its \
         next reconcile pass. Confirm with `st2 tasks --json` — the task's `parked` field clears.",
        catalog.display()
    );
    Ok(())
}

fn print_incomplete_tasks(catalog: PathBuf, host: String, detail: String) -> Result<()> {
    let inventory = st2::task_inventory::TaskInventory::incomplete(catalog, host, detail);
    println!("{}", inventory.to_json());
    anyhow::bail!("task inventory incomplete")
}

fn tool_on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false)
}

fn report_check(problems: &mut usize, ok: bool, label: &str, detail: &str) {
    if ok {
        println!("  ✓ {label}");
    } else {
        *problems += 1;
        if detail.is_empty() {
            println!("  ✗ {label}");
        } else {
            println!("  ✗ {label} — {detail}");
        }
    }
}

fn report_advisory(label: &str, detail: &str) {
    println!("  ⚠ {label} — {detail}");
}

/// A coarse age for an advisory line: an operator reads "12m", not "743109ms".
fn humanize_ms(age_ms: u64) -> String {
    let seconds = age_ms / 1_000;
    match seconds {
        0..=90 => format!("{seconds}s"),
        91..=5_400 => format!("{}m", seconds / 60),
        _ => format!("{}h", seconds / 3_600),
    }
}

fn presentation_cmd(
    field: st2::agent_author::PresentationField,
    args: PresentationArgs,
) -> Result<()> {
    let (selector, requested, json, host) = args.selection()?;
    let root = catalog_arg(None)?;
    let host = host.unwrap_or_else(detect_host);
    let identity = resolve_declaration(&root, &host, selector)?;
    let actor = std::env::var("ST_AGENT")
        .ok()
        .filter(|value| !value.is_empty());
    let requested = requested.as_deref();
    match st2::agent_author::set_presentation(
        &root,
        &identity,
        &host,
        actor.as_deref(),
        field,
        requested,
    ) {
        Ok(receipt) => {
            if json {
                println!("{}", serde_json::to_string(&receipt)?);
            } else {
                let state = match (receipt.result, receipt.value.as_deref()) {
                    (st2::agent_author::AuthorOutcome::Changed, Some(value)) => {
                        format!("set to {value:?}")
                    }
                    (st2::agent_author::AuthorOutcome::Changed, None) => "cleared".to_owned(),
                    (st2::agent_author::AuthorOutcome::Unchanged, Some(value)) => {
                        format!("already {value:?}")
                    }
                    (st2::agent_author::AuthorOutcome::Unchanged, None) => {
                        "already clear".to_owned()
                    }
                };
                println!("{} {}: {state}", receipt.identity, field.as_str());
            }
            Ok(())
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "result": "error",
                        "code": error.code(),
                        "identity": identity,
                        "field": field,
                        "error": error.to_string(),
                    })
                );
            }
            Err(error.into())
        }
    }
}

/// Assign or clear one agent's mutable address (R25) — the third authoring sibling of `st2 rename`
/// and `st2 describe`, classified by exactly the same receipt and refusal vocabulary.
fn address_cmd(args: PresentationArgs) -> Result<()> {
    let (selected, requested, json, host) = args.selection()?;
    let root = catalog_arg(None)?;
    let host = host.unwrap_or_else(detect_host);
    let selector = resolve_declaration(&root, &host, selected)?;
    let actor = std::env::var("ST_AGENT")
        .ok()
        .filter(|value| !value.is_empty());
    let requested = requested.as_deref();
    match st2::agent_author::set_address(&root, &selector, &host, actor.as_deref(), requested) {
        Ok(receipt) => {
            if json {
                println!("{}", serde_json::to_string(&receipt)?);
            } else {
                let state = match (receipt.result, receipt.address.as_deref()) {
                    (st2::agent_author::AuthorOutcome::Changed, Some(value)) => {
                        format!("set to {value:?}")
                    }
                    (st2::agent_author::AuthorOutcome::Changed, None) => "cleared".to_owned(),
                    (st2::agent_author::AuthorOutcome::Unchanged, Some(value)) => {
                        format!("already {value:?}")
                    }
                    (st2::agent_author::AuthorOutcome::Unchanged, None) => {
                        "already clear".to_owned()
                    }
                };
                println!(
                    "{} address: {state} (bus address {})",
                    receipt.id,
                    receipt
                        .bus_address
                        .as_deref()
                        .unwrap_or("none — the subject is retired and non-routable")
                );
            }
            Ok(())
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "result": "error",
                        "code": error.code(),
                        "identity": selector,
                        "field": "address",
                        "error": error.to_string(),
                    })
                );
            }
            Err(error.into())
        }
    }
}

fn desired_state_cmd(
    identity: Option<String>,
    agent_id: Option<String>,
    state: String,
    reason: Option<String>,
    managed_by: Option<String>,
    host: Option<String>,
    json: bool,
) -> Result<()> {
    let state = match state.as_str() {
        "running" => st2::agent_author::DesiredStateValue::Running,
        "suspended" => st2::agent_author::DesiredStateValue::Suspended,
        "retired" => st2::agent_author::DesiredStateValue::Retired,
        _ => unreachable!("clap validates desired state"),
    };
    let root = catalog_arg(None)?;
    let host = host.unwrap_or_else(detect_host);
    let identity = resolve_declaration(&root, &host, one_selector(identity, agent_id)?)?;
    let actor = std::env::var("ST_AGENT")
        .ok()
        .filter(|value| !value.is_empty());
    match st2::agent_author::set_desired_state(
        &root,
        &identity,
        &host,
        actor.as_deref(),
        state,
        reason.as_deref(),
        managed_by.as_deref(),
    ) {
        Ok(receipt) => {
            if json {
                println!("{}", serde_json::to_string(&receipt)?);
            } else {
                println!(
                    "{} desired-state {}{}{} ({})",
                    receipt.identity,
                    receipt.desired_state.as_str(),
                    receipt
                        .reason
                        .as_deref()
                        .map(|reason| format!(" reason={reason:?}"))
                        .unwrap_or_default(),
                    receipt
                        .managed_by
                        .as_deref()
                        .map(|marker| format!(" managed-by={marker:?}"))
                        .unwrap_or_default(),
                    match receipt.result {
                        st2::agent_author::AuthorOutcome::Changed => "changed",
                        st2::agent_author::AuthorOutcome::Unchanged => "unchanged",
                    }
                );
            }
            Ok(())
        }
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "result": "error",
                        "code": error.code(),
                        "identity": identity,
                        "desiredState": state,
                        "error": error.to_string(),
                    })
                );
            }
            Err(error.into())
        }
    }
}

fn status_cmd(
    identity: Option<String>,
    agent_id: Option<String>,
    set: Option<String>,
    ctx: MsgCtx,
) -> Result<()> {
    let (root, host) = resolve_ctx(&ctx)?;
    let id = match selected_route(&root, &host, identity, agent_id)? {
        Some(id) => id,
        None => acting_route(&root, &host, &ctx)?,
    };
    let sp = st2::status::status_path(&agent_dir_of(&root, &id, &host)?);
    match set {
        None => println!("{}", st2::status::read_state(&sp).as_str()),
        Some(word) => {
            let state = st2::status::State::parse_settable(&word).with_context(|| {
                format!("invalid state '{word}' (settable: offline|available|busy|away|dnd)")
            })?;
            message::with_resolved_agent_dir(&root, &route_selector(&id), &host, |agent| {
                st2::status::set_state(&st2::status::status_path(agent), state)
            })?;
            println!("status: {}", state.as_str());
        }
    }
    Ok(())
}

fn agents_cmd(
    catalog: Option<PathBuf>,
    status_filter: Option<String>,
    identity: Option<String>,
    agent_id: Option<String>,
    json: bool,
    enrich: bool,
    mut ctx: MsgCtx,
) -> Result<()> {
    if enrich && !json {
        anyhow::bail!("--enrich requires --json");
    }
    // A positional catalog (like `st2 ls`) takes precedence over `--root`/`$CATALOG`.
    if catalog.is_some() {
        ctx.root = catalog;
    }
    let (root, host) = resolve_ctx(&ctx)?;
    let _catalog_lock = st2::CatalogLock::shared(&root)
        .context("acquire shared catalog-authoring lock for agent roster")?;
    let selected = identity.is_some() || agent_id.is_some();
    let found = if selected {
        st2::discover_strict(&root)
    } else {
        st2::discover(&root)
    };
    if selected && !found.errors.is_empty() {
        let errors = found
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.path.display(), error.message))
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!(
            "cannot select an exact Agent Spec while catalog discovery has {} error(s): {errors}",
            found.errors.len()
        );
    }
    let mut rows = st2::agents::roster_from_discovered(&found, &root, &host);
    if let Some(identity) = &identity {
        rows.retain(|row| row.identity == *identity);
        anyhow::ensure!(
            rows.len() == 1,
            "expected exactly one Agent Spec with identity `{identity}`, found {}",
            rows.len()
        );
    }
    // An exact ID roster query is catalog-global and answers on the appended immutable `id`, not
    // on the positional declaration key — equal bytes in the two namespaces never collide.
    if let Some(agent_id) = &agent_id {
        rows.retain(|row| row.id == *agent_id);
        anyhow::ensure!(
            rows.len() == 1,
            "expected exactly one Agent Spec with agent id `{agent_id}`, found {}",
            rows.len()
        );
    }
    if let Some(f) = &status_filter {
        rows.retain(|r| r.status.as_str() == f);
        if let Some(identity) = &identity {
            anyhow::ensure!(
                rows.len() == 1,
                "Agent Spec `{identity}` does not match status `{f}`"
            );
        }
    }
    if json {
        println!("{}", st2::agents::to_json(&rows, enrich));
    } else {
        for r in &rows {
            let lifecycle = if r.desired_state == "running" {
                String::new()
            } else {
                format!(
                    "\t[{}{}]",
                    r.desired_state,
                    r.desired_state_reason
                        .as_deref()
                        .map(|reason| format!(": {reason}"))
                        .unwrap_or_default()
                )
            };
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}{}",
                r.identity,
                r.status.as_str(),
                observed_column(r.observed.as_ref()),
                context_column(r.context.as_ref()),
                r.name.as_deref().unwrap_or(""),
                r.description.as_deref().unwrap_or(""),
                lifecycle,
            );
        }
    }
    Ok(())
}

/// The compact observed-harness-state column for human `st2 agents` output. `-` means no record
/// exists (no driver has published one), which is distinct from a derived `unknown`.
fn observed_column(observed: Option<&st2::harness_state::Observed>) -> String {
    // Prefixed so the observed word is never mistaken for the declared presence beside it —
    // two bare state words in one row is the exact ambiguity the ontology's collision rules name.
    let Some(observed) = observed else {
        return "obs:-".to_string();
    };
    let mut column = observed.state.as_str().to_string();
    if observed.blocked_on == st2::harness_state::BlockedOn::Human {
        column.push_str("+human");
    }
    if observed.state == st2::harness_state::Activity::Unknown
        && let Some(reason) = observed.reason.as_deref()
    {
        column = format!("unknown({reason})");
    }
    format!("obs:{column}")
}

/// The compact harness-context column for human `st2 agents` output. `-` means no record exists;
/// `?` means a record exists whose percent the harness withheld — Claude before its session's
/// first API response, pi across a compaction — and the two must not render alike, or the column
/// says "nobody is watching" for a producer that is watching and honestly does not know.
fn context_column(context: Option<&st2::harness_context::Observed>) -> String {
    // Prefixed like the observed column beside it, so neither compact word is mistaken for the
    // other or for declared presence.
    let Some(context) = context else {
        return "ctx:-".to_string();
    };
    // The harness's own percent, rounded for width and never clamped: a reading above 100 is a
    // real overrun and the one an operator most needs to see.
    let mut column = match context.used_percent {
        Some(percent) => format!("{}%", percent.round()),
        None => "?".to_string(),
    };
    if context.is_rate_limited() {
        column.push_str(" rate-limited");
    }
    if context.compactions > 0 {
        column.push_str(&format!(" ⟳{}", context.compactions));
    }
    if context.stale {
        column.push_str(" stale");
    }
    format!("ctx:{column}")
}

fn ding_cmd(
    session: Option<String>,
    identity: Option<String>,
    agent_id: Option<String>,
    root: Option<PathBuf>,
    host: Option<String>,
    interval: u64,
) -> Result<()> {
    let ctx = MsgCtx {
        root,
        as_id: identity,
        host,
    };
    let (catalog_root, this_host) = resolve_ctx(&ctx)?;
    // The poked pty session is a runtime task ID, so it keeps the exact bytes the caller named:
    // under activation `ST_AGENT` carries the agent ID and the canonical agent task ID equals it.
    // The inbox and status are declaration-parent state, so they resolve through the route.
    let named = match &agent_id {
        Some(id) => id.clone(),
        None => acting_id(&ctx)?,
    };
    let id = match agent_id {
        Some(id) => resolve_route(
            &catalog_root,
            &this_host,
            st2::identity::AgentSelector::Id(id),
        )?,
        None => acting_route(&catalog_root, &this_host, &ctx)?,
    };
    // The pty to poke defaults to the identity — an agent IS its pty, so the session id == the agent
    // id. So `st2 ding --identity mix.worker` pokes pty `mix.worker` (the redundant positional is now
    // optional). An explicit positional still overrides for the rare non-agent case.
    let session = session.unwrap_or(named);
    // Flat-bus aware: a native catalog agent → its resources/inbox; a catalog-LESS bus (an eval's
    // ST_ROOT) → the flat <root>/<id>/inbox. Status lives beside it either way.
    let agent_dir = message::resolve_agent_dir(&catalog_root, &route_selector(&id), &this_host)?
        .unwrap_or_else(|| catalog_root.join(&id));
    let inbox = resolve_message_inbox(&catalog_root, &id, &this_host)?;
    let status_path = st2::status::status_path(&agent_dir);
    eprintln!(
        "st2 ding: watching {}'s inbox ({}) → poking pty '{session}'",
        id,
        inbox.display()
    );
    let config = ding::DingConfig {
        poll: Duration::from_millis(interval),
        ..Default::default()
    };
    ding::serve(
        &catalog_root,
        &this_host,
        &id,
        &inbox,
        &status_path,
        &session,
        &config,
    )
}

/// Resolve the catalog root and local host from a message subcommand's shared context.
fn resolve_ctx(ctx: &MsgCtx) -> Result<(PathBuf, String)> {
    let root = match &ctx.root {
        Some(root) => absolute_catalog_path(root)?,
        None => catalog_root_for_env()?,
    };
    let host = ctx.host.clone().unwrap_or_else(detect_host);
    Ok((root, host))
}

/// The acting identity (`from` / whose inbox is "mine"): `--as`, else `$ST_AGENT`.
fn acting_id(ctx: &MsgCtx) -> Result<String> {
    ctx.as_id
        .clone()
        .or_else(|| std::env::var("ST_AGENT").ok())
        .filter(|s| !s.is_empty())
        .context("no acting identity: pass --as or set $ST_AGENT")
}

/// The acting subject's route.
///
/// `--as` is an ordinary address reference; `$ST_AGENT` carries the exact immutable agent ID
/// (`docs/vrs/03-message/spec.md`: selection follows one total order, and neither an address nor
/// `ST_AGENT` is heuristically retyped). So the two are not interchangeable strings once a subject
/// declares an explicit address. An ID that names no declared subject keeps its raw bytes, which
/// is what leaves flat, orphan, and `$ST2_EVAL_REQUESTER` mailboxes resolving exactly as they do
/// today; ambiguity cannot arise for a catalog-global ID, so absence is the only miss.
fn acting_route(root: &Path, host: &str, ctx: &MsgCtx) -> Result<String> {
    if let Some(address) = ctx.as_id.clone().filter(|value| !value.is_empty()) {
        return Ok(address);
    }
    let id = acting_id(ctx)?;
    match resolve_selected(root, host, st2::identity::AgentSelector::Id(id.clone())) {
        Ok(selected) => Ok(selected.route),
        Err(_) => Ok(id),
    }
}

/// One already-resolved route as the selector every message-plane entry point takes.
///
/// A route is an effective address by construction — `resolve_route` produced it from the address
/// book — so this states that namespace explicitly instead of letting a bare string be retyped.
fn route_selector(reference: &str) -> st2::identity::AgentSelector {
    st2::identity::AgentSelector::Address(reference.to_owned())
}

/// Resolve a recipient/identity to its agent folder in the catalog, or a clear error.
fn agent_dir_of(root: &Path, id: &str, host: &str) -> Result<PathBuf> {
    message::resolve_agent_dir(root, &route_selector(id), host)?
        .with_context(|| format!("no agent '{id}' found in catalog {}", root.display()))
}

/// The one typed agent selector a CLI reference pair carries (R24).
///
/// Every agent-selecting command exposes both forms and clap's `conflicts_with` keeps them
/// mutually exclusive, so at most one arrives here: an ordinary address reference, or an exact
/// immutable agent ID.
fn agent_selector(
    reference: Option<String>,
    id: Option<String>,
) -> Option<st2::identity::AgentSelector> {
    match (reference, id) {
        (_, Some(id)) => Some(st2::identity::AgentSelector::Id(id)),
        (Some(reference), None) => Some(st2::identity::AgentSelector::Address(reference)),
        (None, None) => None,
    }
}

/// [`agent_selector`] for a command that requires a target rather than defaulting to the actor.
fn one_selector(
    reference: Option<String>,
    id: Option<String>,
) -> Result<st2::identity::AgentSelector> {
    agent_selector(reference, id)
        .context("no agent selected: pass an agent reference or the exact `--id` form")
}

/// The one subject a typed selector names, in both namespaces its consumers accept.
///
/// These are two different strings once a subject declares an explicit `address`, and handing the
/// wrong one onward is exactly the ID-through-a-mutable-address hop decision 0015-immutable-agent-id-and-mutable-address forbids:
/// declaration-selecting commands match the positional key and never look at `address`, while
/// reference resolution for inboxes, status, context, and recipients answers on the address.
struct SelectedAgent {
    /// `<host>.<identity>` — the positional declaration key, which no address cutover changes.
    declaration: String,
    /// `<host>.<effective address>` — the current route.
    route: String,
}

/// Resolve one typed selector against the catalog.
///
/// The address form stays on st2's existing reference resolution, which DELTA-003 keeps normative
/// until the identity model activates, so it carries the caller's bytes through unchanged. The ID
/// form is a catalog-global exact lookup that never falls through to address lookup — that is what
/// stops a renamed subject's old semantic ID from silently staying alive as a route.
fn resolve_selected(
    root: &Path,
    host: &str,
    selector: st2::identity::AgentSelector,
) -> Result<SelectedAgent> {
    let id = match selector {
        st2::identity::AgentSelector::Address(reference) => {
            return Ok(SelectedAgent {
                declaration: reference.clone(),
                route: reference,
            });
        }
        st2::identity::AgentSelector::Id(id) => id,
    };
    let found = discover(root);
    // Retired subjects are present here and absent from the routable address book on purpose:
    // retirement releases the address, never the ID, so an exact-ID selector still names its
    // subject and `st2 describe` can still edit a retired declaration.
    let entries = found
        .specs
        .iter()
        .map(|spec| st2::identity::AddressBookEntry::of(spec, host))
        .collect::<Vec<_>>();
    let resolved = &st2::identity::resolve_id(&entries, &id)?.id;
    let spec = found
        .specs
        .iter()
        .find(|spec| &spec.effective_id(host) == resolved)
        .context("the resolved agent id left the discovery it was resolved against")?;
    Ok(SelectedAgent {
        declaration: spec.bus_id(host),
        route: spec.bus_address(host),
    })
}

/// [`resolve_selected`] for a command that selects a declaration to read or author.
fn resolve_declaration(
    root: &Path,
    host: &str,
    selector: st2::identity::AgentSelector,
) -> Result<String> {
    resolve_selected(root, host, selector).map(|selected| selected.declaration)
}

/// [`resolve_selected`] for a command that resolves a route: an inbox, status, or recipient.
fn resolve_route(
    root: &Path,
    host: &str,
    selector: st2::identity::AgentSelector,
) -> Result<String> {
    resolve_selected(root, host, selector).map(|selected| selected.route)
}

/// The declaration selector for one CLI reference pair, or `None` when the command must fall back
/// to the caller's own ambient actor.
fn selected_declaration(
    root: &Path,
    host: &str,
    reference: Option<String>,
    id: Option<String>,
) -> Result<Option<String>> {
    match agent_selector(reference, id) {
        None => Ok(None),
        Some(selector) => resolve_declaration(root, host, selector).map(Some),
    }
}

/// [`selected_declaration`] for a command that resolves a route rather than a declaration.
fn selected_route(
    root: &Path,
    host: &str,
    reference: Option<String>,
    id: Option<String>,
) -> Result<Option<String>> {
    match agent_selector(reference, id) {
        None => Ok(None),
        Some(selector) => resolve_route(root, host, selector).map(Some),
    }
}

/// Resolve ordinary declared messaging authority plus the exact external requester capability
/// injected only into canonical eval seats.
fn resolve_message_inbox(root: &Path, id: &str, host: &str) -> Result<PathBuf> {
    let external = std::env::var("ST2_EVAL_REQUESTER")
        .ok()
        .map(|identity| message::ExternalInbox::new(root, &identity))
        .transpose()?;
    message::resolve_inbox_with_external(root, id, host, external.as_ref())
}

/// Body from `-m`, else stdin (so `st2 message send x < file` works).
fn body_or_stdin(body: Option<String>) -> Result<String> {
    let body = match body {
        Some(body) => body,
        None => {
            use std::io::Read as _;
            let mut body = String::new();
            std::io::stdin()
                .read_to_string(&mut body)
                .context("reading message body from stdin")?;
            body
        }
    };
    if body.is_empty() {
        anyhow::bail!("message body must not be empty");
    }
    Ok(body)
}

/// `[identity] <filename>` positionals: if `second` is present, `first` is the identity; otherwise
/// `first` is the filename and the box belongs to `--id`, else to the acting identity.
fn box_target(
    first: String,
    second: Option<String>,
    owner: Option<String>,
    mine: impl FnOnce() -> Result<String>,
) -> Result<(String, String)> {
    match (second, owner) {
        (Some(thing), _) => Ok((first, thing)),
        (None, Some(owner)) => Ok((owner, first)),
        (None, None) => Ok((mine()?, first)),
    }
}

fn message_cmd(cmd: MessageCmd) -> Result<()> {
    match cmd {
        MessageCmd::Send {
            to,
            to_id,
            body,
            subject,
            in_reply_to,
            tags,
            idempotency_key,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let to = resolve_route(&root, &host, one_selector(to, to_id)?)?;
            let from = acting_route(&root, &host, &ctx)?;
            let body = body_or_stdin(body)?;
            let filename = send_resolved_message(
                &root,
                &to,
                &host,
                &from,
                subject.as_deref(),
                in_reply_to.as_deref(),
                &tags,
                &body,
                idempotency_key.as_deref(),
            )?;
            println!("{filename}");
            Ok(())
        }
        MessageCmd::Reply {
            filename,
            body,
            subject,
            idempotency_key,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let from = acting_route(&root, &host, &ctx)?;
            let my_inbox = resolve_message_inbox(&root, &from, &host)?;
            let original = message::read_msg(&my_inbox, &filename)
                .or_else(|inbox_error| {
                    if my_inbox.join(&filename).try_exists()? {
                        return Err(inbox_error);
                    }
                    let my_archive = message::resolve_archive(&root, &from, &host)?;
                    message::read_msg(&my_archive, &filename)
                })
                .with_context(|| format!("no message '{filename}' in {}'s inbox", from))?;
            // The reply target is the message's own `from`: an ordinary address reference, which
            // is what every published record carries.
            let to = original
                .from
                .clone()
                .with_context(|| format!("message '{filename}' has no `from` to reply to"))?;
            let subject = subject.or_else(|| message::reply_subject(original.subject.as_deref()));
            let body = body_or_stdin(body)?;
            let sent = send_resolved_message(
                &root,
                &to,
                &host,
                &from,
                subject.as_deref(),
                Some(&filename),
                &[],
                &body,
                idempotency_key.as_deref(),
            )?;
            println!("{sent}");
            Ok(())
        }
        MessageCmd::Sent {
            identity,
            agent_id,
            count,
            include_body,
            to,
            since,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let id = match selected_route(&root, &host, identity, agent_id)? {
                Some(id) => id,
                None => acting_route(&root, &host, &ctx)?,
            };
            let mut view =
                message::with_resolved_agent_dir(&root, &route_selector(&id), &host, |agent_dir| {
                message::list_sent(agent_dir, include_body)
            })?;
            if let Some(recipient) = &to {
                view.messages.retain(|message| message.to == *recipient);
            }
            if let Some(cursor) = since {
                view.messages.retain(|message| message.ts > cursor);
            }
            if count {
                match view.coverage {
                    st2_wire::message::SentCoverage::Since { .. } => {
                        println!("{}", view.messages.len());
                        return Ok(());
                    }
                    st2_wire::message::SentCoverage::Unavailable => {
                        anyhow::bail!("sent-message coverage is unavailable")
                    }
                    st2_wire::message::SentCoverage::Partial { pending, .. } => {
                        anyhow::bail!("sent-message coverage is partial ({pending} pending)")
                    }
                }
            }
            if json {
                println!("{}", serde_json::to_string(&view)?);
                return Ok(());
            }
            let coverage = match view.coverage {
                st2_wire::message::SentCoverage::Unavailable => "unavailable".to_string(),
                st2_wire::message::SentCoverage::Since { since } => format!("since {since}"),
                st2_wire::message::SentCoverage::Partial { since, pending } => {
                    format!("partial since {since}; {pending} pending")
                }
            };
            println!(
                "# {} sent message{} for {id} ({coverage})",
                view.messages.len(),
                plural(view.messages.len())
            );
            for message in &view.messages {
                let subject = message.subject.as_deref().unwrap_or("(no subject)");
                println!("{}  to {}  {subject}", message.filename, message.to);
            }
            Ok(())
        }
        MessageCmd::Ls {
            identity,
            agent_id,
            archive,
            orphan,
            count,
            include_body,
            from,
            since,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let id = match selected_route(&root, &host, identity, agent_id)? {
                Some(id) => id,
                None => acting_route(&root, &host, &ctx)?,
            };
            let dir = message::resolve_list_box(&root, &id, &host, archive, orphan)?;
            let mut msgs = if archive {
                message::list_dir(&dir)?
            } else {
                message::list_inbox(&dir)?
            };
            if let Some(sender) = &from {
                msgs.retain(|m| m.from.as_deref() == Some(sender.as_str()));
            }
            if let Some(cursor) = since {
                msgs.retain(|m| m.ts_ms > cursor);
            }
            if count {
                println!("{}", msgs.len());
                return Ok(());
            }
            if json {
                let items: Vec<LsItemJson> = msgs
                    .iter()
                    .map(|m| LsItemJson::from_message(m, include_body))
                    .collect();
                println!("{}", serde_json::to_string(&items)?);
                return Ok(());
            }
            let box_name = if archive { "archive" } else { "inbox" };
            println!(
                "# {} message{} in {id} {box_name}",
                msgs.len(),
                plural(msgs.len())
            );
            for m in &msgs {
                let from = m.from.as_deref().unwrap_or("<unknown>");
                let subj = m.subject.as_deref().unwrap_or("(no subject)");
                println!("{}  from {from}  {subj}", m.filename);
            }
            Ok(())
        }
        MessageCmd::Read {
            first,
            second,
            agent_id,
            archive,
            raw,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let owner = selected_route(&root, &host, None, agent_id)?;
            let (id, filename) =
                box_target(first, second, owner, || acting_route(&root, &host, &ctx))?;
            let dir = if archive {
                message::resolve_archive(&root, &id, &host)
            } else {
                resolve_message_inbox(&root, &id, &host)
            }?;
            if raw {
                print!("{}", std::fs::read_to_string(dir.join(&filename))?);
                return Ok(());
            }
            let m = message::read_msg(&dir, &filename)?;
            if json {
                println!("{}", serde_json::to_string(&MessageJson::from(&m))?);
                return Ok(());
            }
            println!("from:        {}", m.from.as_deref().unwrap_or("<unknown>"));
            if let Some(s) = &m.subject {
                println!("subject:     {s}");
            }
            if let Some(irt) = &m.in_reply_to {
                println!("in-reply-to: {irt}");
            }
            if !m.tags.is_empty() {
                println!("tags:        {}", m.tags.join(", "));
            }
            println!();
            print!("{}", m.body);
            Ok(())
        }
        MessageCmd::Archive {
            first,
            second,
            agent_id,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let owner = selected_route(&root, &host, None, agent_id)?;
            let (id, filename) =
                box_target(first, second, owner, || acting_route(&root, &host, &ctx))?;
            message::archive_resolved_message(&root, &route_selector(&id), &host, &filename)?;
            println!("archived");
            Ok(())
        }
        MessageCmd::Thread {
            first,
            second,
            tree,
            ctx,
        } => {
            let (root, _host) = resolve_ctx(&ctx)?;
            // `[identity] filename` — the identity is irrelevant (the walk is catalog-wide).
            let filename = second.unwrap_or(first);
            let mut entries = message::collect_thread(&root, &filename)?;
            if entries.is_empty() {
                anyhow::bail!(
                    "no thread found for '{filename}' in catalog {}",
                    root.display()
                );
            }
            if !tree {
                entries.sort_by_key(|e| e.ts_ms); // flat chronological
            }
            for e in &entries {
                let indent = if tree {
                    "  ".repeat(e.depth)
                } else {
                    String::new()
                };
                let from = e.from.as_deref().unwrap_or("<unknown>");
                let subj = e.subject.as_deref().unwrap_or("(no subject)");
                println!("{indent}{}  from {from}  {subj}", e.filename);
            }
            Ok(())
        }
    }
}

fn send_resolved_message(
    root: &Path,
    to: &str,
    host: &str,
    from: &str,
    subject: Option<&str>,
    in_reply_to: Option<&str>,
    tags: &[String],
    body: &str,
    idempotency_key: Option<&str>,
) -> Result<String> {
    let external = std::env::var("ST2_EVAL_REQUESTER")
        .ok()
        .map(|identity| message::ExternalInbox::new(root, &identity))
        .transpose()?;
    message::send_to_resolved_inbox(
        root,
        &route_selector(to),
        host,
        &route_selector(from),
        subject,
        in_reply_to,
        tags,
        body,
        idempotency_key,
        external.as_ref(),
    )
}

fn event_cmd(cmd: EventCmd) -> Result<()> {
    match cmd {
        EventCmd::Emit {
            recipient,
            recipient_id,
            stream,
            event_id,
            key,
            supersede,
            subject,
            body,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let recipient =
                resolve_route(&root, &host, one_selector(recipient, recipient_id)?)?;
            let body = body_or_stdin(body)?;
            let receipt = st2::event::emit(
                &root,
                &host,
                &recipient,
                &stream,
                &event_id,
                key.as_deref(),
                subject.as_deref(),
                &body,
                supersede,
            )?;
            if json {
                println!("{}", serde_json::to_string(&receipt)?);
            } else {
                println!("{}", receipt.filename);
            }
            Ok(())
        }
    }
}

fn stream_cmd(cmd: StreamCmd) -> Result<()> {
    let (name, agent, agent_id, json, ctx, launch, remove) = match cmd {
        StreamCmd::Add {
            name,
            agent,
            agent_id,
            command,
            adapter_argv,
            json,
            ctx,
        } => {
            let launch = match (command, adapter_argv.is_empty()) {
                (Some(command), true) => Some(agent_spec::StreamLaunch::Command(command)),
                (None, false) => Some(agent_spec::StreamLaunch::Argv(adapter_argv)),
                (None, true) => None,
                (Some(_), false) => anyhow::bail!("stream add got both --command and adapter argv"),
            };
            (name, agent, agent_id, json, ctx, launch, false)
        }
        StreamCmd::Rm {
            name,
            agent,
            agent_id,
            json,
            ctx,
        } => (name, agent, agent_id, json, ctx, None, true),
    };
    let (root, host) = resolve_ctx(&ctx)?;
    let actor = ctx
        .as_id
        .clone()
        .or_else(|| std::env::var("ST_AGENT").ok())
        .filter(|value| !value.is_empty());
    let target = match selected_declaration(&root, &host, agent, agent_id)? {
        Some(target) => target,
        None => actor
            .clone()
            .context("no stream target: pass --agent, --agent-id, --as, or set $ST_AGENT")?,
    };
    if remove {
        let receipt =
            st2::agent_author::remove_stream(&root, &target, &host, actor.as_deref(), &name)?;
        if json {
            println!("{}", serde_json::to_string(&receipt)?);
        } else {
            println!(
                "{:?} stream {} on {}",
                receipt.result, receipt.name, receipt.identity
            );
        }
    } else {
        let receipt =
            st2::agent_author::add_stream(&root, &target, &host, actor.as_deref(), &name, launch)?;
        if json {
            println!("{}", serde_json::to_string(&receipt)?);
        } else {
            println!(
                "{:?} stream {} on {}",
                receipt.result, receipt.name, receipt.identity
            );
        }
    }
    Ok(())
}

fn request_cmd(cmd: RequestCmd) -> Result<()> {
    match cmd {
        RequestCmd::Send {
            to,
            idempotency_key,
            tags,
            body,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let principal = acting_id(&ctx)?;
            let body = parse_json_body(body)?;
            let receipt = st2::request::publish(
                &root,
                &host,
                &principal,
                &to,
                &idempotency_key,
                parse_typed_tags(tags)?,
                body,
            )?;
            print_publish_receipt(&receipt, json)
        }
        RequestCmd::Reply {
            request_filename,
            tags,
            body,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let agent = acting_id(&ctx)?;
            let body = parse_json_body(body)?;
            let receipt = st2::request::reply(
                &root,
                &host,
                &agent,
                &request_filename,
                parse_typed_tags(tags)?,
                body,
            )?;
            print_publish_receipt(&receipt, json)
        }
        RequestCmd::Read {
            request_filename,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let agent = acting_id(&ctx)?;
            let request = st2::request::read(&root, &host, &agent, &request_filename)?;
            if json {
                println!("{}", serde_json::to_string(&request)?);
            } else {
                println!(
                    "request {} from {} ({})",
                    request.idempotency_key, request.from, request.request_filename
                );
            }
            Ok(())
        }
        RequestCmd::Status {
            idempotency_key,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let principal = acting_id(&ctx)?;
            let status = st2::request::status(&root, &host, &principal, &idempotency_key)?;
            if json {
                println!("{}", serde_json::to_string(&status)?);
            } else {
                match status {
                    st2::request::RequestStatus::Pending {
                        idempotency_key,
                        request_filename,
                    } => println!("pending {idempotency_key} ({request_filename})"),
                    st2::request::RequestStatus::Replied {
                        idempotency_key,
                        request_filename,
                        from,
                        ..
                    } => println!("replied {idempotency_key} ({request_filename}) from {from}"),
                }
            }
            Ok(())
        }
    }
}

fn parse_json_body(body: Option<String>) -> Result<serde_json::Value> {
    let body = body_or_stdin(body)?;
    serde_json::from_str(&body).context("request body must be valid JSON")
}

fn parse_typed_tags(tags: Vec<String>) -> Result<std::collections::BTreeMap<String, String>> {
    let mut parsed = std::collections::BTreeMap::new();
    for tag in tags {
        let (key, value) = tag
            .split_once('=')
            .with_context(|| format!("typed tag must be `key=value`, got `{tag}`"))?;
        if key.is_empty() || value.is_empty() {
            anyhow::bail!("typed tag must have a non-empty key and value: `{tag}`");
        }
        if parsed.insert(key.to_string(), value.to_string()).is_some() {
            anyhow::bail!("duplicate typed tag key `{key}`");
        }
    }
    Ok(parsed)
}

fn print_publish_receipt(receipt: &st2::request::PublishReceipt, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(receipt)?);
    } else {
        println!("{}", receipt.filename);
    }
    Ok(())
}

/// `st2 message ls --json` row (stable st2 wire contract).
#[derive(serde::Serialize)]
struct LsItemJson<'a> {
    filename: &'a str,
    ts: u64,
    from: Option<&'a str>,
    subject: Option<&'a str>,
    #[serde(rename = "inReplyTo")]
    in_reply_to: Option<&'a str>,
    tags: &'a [String],
    priority: Option<&'a str>,
    #[serde(rename = "idempotencyKey", skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<&'a str>,
    #[serde(rename = "eventId", skip_serializing_if = "Option::is_none")]
    event_id: Option<&'a str>,
    #[serde(rename = "eventKey", skip_serializing_if = "Option::is_none")]
    event_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
}

impl<'a> From<&'a st2::message::Message> for LsItemJson<'a> {
    fn from(m: &'a st2::message::Message) -> Self {
        LsItemJson {
            filename: &m.filename,
            ts: m.ts_ms,
            from: m.from.as_deref(),
            subject: m.subject.as_deref(),
            in_reply_to: m.in_reply_to.as_deref(),
            tags: &m.tags,
            priority: m.priority.as_deref(),
            idempotency_key: m.idempotency_key.as_deref(),
            stream: m.stream.as_deref(),
            event_id: m.event_id.as_deref(),
            event_key: m.event_key.as_deref(),
            body: None,
        }
    }
}

impl<'a> LsItemJson<'a> {
    fn from_message(m: &'a st2::message::Message, include_body: bool) -> Self {
        let mut item = Self::from(m);
        if include_body {
            item.body = Some(&m.body);
        }
        item
    }
}

/// `st2 message read --json` — the full message.
#[derive(serde::Serialize)]
struct MessageJson<'a> {
    filename: &'a str,
    ts: u64,
    from: Option<&'a str>,
    subject: Option<&'a str>,
    #[serde(rename = "inReplyTo")]
    in_reply_to: Option<&'a str>,
    tags: &'a [String],
    priority: Option<&'a str>,
    #[serde(rename = "idempotencyKey", skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<&'a str>,
    #[serde(rename = "eventId", skip_serializing_if = "Option::is_none")]
    event_id: Option<&'a str>,
    #[serde(rename = "eventKey", skip_serializing_if = "Option::is_none")]
    event_key: Option<&'a str>,
    body: &'a str,
}

impl<'a> From<&'a st2::message::Message> for MessageJson<'a> {
    fn from(m: &'a st2::message::Message) -> Self {
        MessageJson {
            filename: &m.filename,
            ts: m.ts_ms,
            from: m.from.as_deref(),
            subject: m.subject.as_deref(),
            in_reply_to: m.in_reply_to.as_deref(),
            tags: &m.tags,
            priority: m.priority.as_deref(),
            idempotency_key: m.idempotency_key.as_deref(),
            stream: m.stream.as_deref(),
            event_id: m.event_id.as_deref(),
            event_key: m.event_key.as_deref(),
            body: &m.body,
        }
    }
}

fn context_cmd(cmd: ContextCmd) -> Result<()> {
    use st2::context::{self, View};
    match cmd {
        ContextCmd::Read {
            identity,
            agent_id,
            decisions,
            full,
            fresh_within,
            ctx,
        } => {
            let dir = resolve_context_dir(identity, agent_id, &ctx)?;
            let view = if full {
                View::Full
            } else if decisions {
                View::Decisions
            } else {
                View::Now
            };
            if fresh_within.is_some() && view != View::Now {
                anyhow::bail!("--fresh-within cannot be combined with --decisions or --full");
            }
            let content = match fresh_within {
                Some(seconds) => context::read_now_fresh(&dir, Duration::from_secs(seconds)),
                None => context::read(&dir, view),
            };
            print!("{content}");
            Ok(())
        }
        ContextCmd::Write {
            identity,
            agent_id,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let id = match selected_route(&root, &host, identity, agent_id)? {
                Some(identity) => identity,
                None => acting_route(&root, &host, &ctx)?,
            };
            let content =
                std::io::read_to_string(std::io::stdin()).context("reading context from stdin")?;
            message::with_resolved_state_dir(
                &root,
                &route_selector(&id),
                &host,
                &["resources", "context"],
                true,
                |dir| context::write_now(dir, &content),
            )?;
            eprintln!("context: wrote now.md ({} bytes)", content.len());
            Ok(())
        }
        ContextCmd::Append {
            identity,
            agent_id,
            decision,
            why,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let id = match selected_route(&root, &host, identity, agent_id)? {
                Some(identity) => identity,
                None => acting_route(&root, &host, &ctx)?,
            };
            let filename = message::with_resolved_state_dir(
                &root,
                &route_selector(&id),
                &host,
                &["resources", "context", "decisions"],
                true,
                |dir| context::append_decision_to_dir(dir, &decision, &why),
            )?;
            println!("{filename}");
            Ok(())
        }
    }
}

fn service_cmd(cmd: ServiceCmd) -> Result<()> {
    match cmd {
        ServiceCmd::Install {
            catalog,
            host,
            pty_root,
            memory_max_mb,
        } => {
            let catalog = match catalog {
                Some(c) => c,
                None => catalog_root_for_env()?,
            };
            st2::service::install(&catalog, host, pty_root, memory_max_mb)
        }
        ServiceCmd::Status => st2::service::status(),
        ServiceCmd::Uninstall => st2::service::uninstall(),
    }
}

fn claude_channel_cmd(cmd: ClaudeChannelCmd) -> Result<()> {
    match cmd {
        ClaudeChannelCmd::Install { no_policy } => {
            st2::claude_channel::install(no_policy).map(|_| ())
        }
        ClaudeChannelCmd::Status => st2::claude_channel::status(),
        ClaudeChannelCmd::Uninstall { keep_policy } => st2::claude_channel::uninstall(keep_policy),
        ClaudeChannelCmd::InstallPolicy => st2::claude_channel::install_policy().map(|_| ()),
        ClaudeChannelCmd::UninstallPolicy => st2::claude_channel::uninstall_policy(),
    }
}

/// Read one agent's declared Resource bindings. Selector resolution mirrors the mediated author
/// (`bus_id` first, then bare identity, unique or refuse) so `ls` and `add` always name the same
/// declaration, and a malformed catalog refuses rather than silently hiding an agent.
fn resource_bindings(
    root: &Path,
    selector: &str,
    host: &str,
) -> Result<(String, Vec<st2::Resource>)> {
    with_resource_bindings_snapshot(root, selector, host, |identity, bindings| {
        Ok((identity, bindings))
    })
}

/// Resolve one binding projection and let the caller finish consuming that exact catalog snapshot
/// before its shared authoring fence is released.
fn with_resource_bindings_snapshot<T>(
    root: &Path,
    selector: &str,
    host: &str,
    consume: impl FnOnce(String, Vec<st2::Resource>) -> Result<T>,
) -> Result<T> {
    let _catalog_lock = st2::CatalogLock::shared(root)
        .context("acquire shared catalog-authoring lock for Resource bindings")?;
    let found = st2::discover_strict(root);
    if let Some(error) = found.errors.first() {
        anyhow::bail!(
            "cannot prove an exact Resource binding target while {} is malformed: {}",
            error.path.display(),
            error.message
        );
    }
    let exact = found
        .specs
        .iter()
        .filter(|spec| spec.bus_id(host) == selector)
        .collect::<Vec<_>>();
    let matches = if exact.is_empty() {
        found
            .specs
            .iter()
            .filter(|spec| spec.identity == selector)
            .collect::<Vec<_>>()
    } else {
        exact
    };
    let (identity, bindings) = match matches.as_slice() {
        [] => anyhow::bail!("no agent '{selector}' found in catalog {}", root.display()),
        [spec] => (spec.bus_id(host), spec.resources.clone()),
        many => {
            let mut candidates = many
                .iter()
                .map(|spec| format!("{} ({})", spec.bus_id(host), spec.path.display()))
                .collect::<Vec<_>>();
            candidates.sort();
            anyhow::bail!(
                "agent selector '{selector}' is ambiguous: {}",
                candidates.join(", ")
            )
        }
    };
    consume(identity, bindings)
}

#[cfg(debug_assertions)]
fn resource_refresh_snapshot_checkpoint() {
    let (Ok(ready), Ok(release)) = (
        std::env::var("ST2_TEST_RESOURCE_REFRESH_SNAPSHOT_READY"),
        std::env::var("ST2_TEST_RESOURCE_REFRESH_SNAPSHOT_RELEASE"),
    ) else {
        return;
    };
    let _ = std::fs::write(ready, b"ready");
    while !Path::new(&release).exists() {
        std::thread::yield_now();
    }
}

#[cfg(not(debug_assertions))]
fn resource_refresh_snapshot_checkpoint() {}

fn resource_cmd(cmd: ResourceCmd) -> Result<()> {
    match cmd {
        ResourceCmd::Ls {
            identity,
            agent_id,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let id = match selected_declaration(&root, &host, identity, agent_id)? {
                Some(id) => id,
                None => acting_id(&ctx)?,
            };
            let (identity, bindings) = resource_bindings(&root, &id, &host)?;
            if json {
                println!("{}", serde_json::to_string(&bindings)?);
                return Ok(());
            }
            println!(
                "# {} resource{} for {identity}",
                bindings.len(),
                plural(bindings.len())
            );
            // Align the uri column to the widest name so a long binding name still leaves a
            // separator. `{:<width$}` pads by chars, so measure chars, not bytes.
            let width = bindings
                .iter()
                .map(|binding| binding.name().chars().count())
                .max()
                .unwrap_or(0)
                + 2;
            for binding in &bindings {
                println!("  {:<width$}{}", binding.name(), binding.uri());
            }
            Ok(())
        }
        ResourceCmd::Read {
            first,
            second,
            agent_id,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let owner = selected_declaration(&root, &host, None, agent_id)?;
            let (id, name) = box_target(first, second, owner, || acting_id(&ctx))?;
            let (identity, bindings) = resource_bindings(&root, &id, &host)?;
            let binding = bindings
                .iter()
                .find(|binding| binding.name() == name)
                .with_context(|| format!("no resource binding '{name}' declared by {identity}"))?;
            if json {
                println!("{}", serde_json::to_string(binding)?);
                return Ok(());
            }
            println!("{:<17}{}", "name:", binding.name());
            println!("{:<17}{}", "uri:", binding.uri());
            println!("{:<17}{}", "reason:", binding.reason());
            if let Some(inactive_reason) = binding.inactive_reason() {
                println!("{:<17}{}", "inactive-reason:", inactive_reason);
            }
            if let Some(selector) = binding.selector() {
                println!("{:<17}{}", "selector:", serde_json::to_string(selector)?);
            }
            Ok(())
        }
        ResourceCmd::Refresh {
            first,
            second,
            agent,
            agent_id,
            wait,
            json,
            ctx,
        } => {
            let (root, host) = resolve_ctx(&ctx)?;
            let (selector, name) = match second {
                Some(name) => (first, name),
                None => {
                    let selector = match selected_declaration(&root, &host, agent, agent_id)? {
                        Some(agent) => agent,
                        None => acting_id(&ctx)?,
                    };
                    (selector, first)
                }
            };
            let (identity, request) =
                with_resource_bindings_snapshot(&root, &selector, &host, |identity, bindings| {
                    bindings
                        .iter()
                        .find(|binding| binding.name() == name)
                        .with_context(|| {
                            format!("no resource binding '{name}' declared by {identity}")
                        })?;
                    resource_refresh_snapshot_checkpoint();
                    let generation = st2::resource_observe::catalog_generation(&root)?;
                    let request = st2::resource_observe::ObserveRequest::new(
                        identity.clone(),
                        name.clone(),
                        generation,
                        None,
                    )?;
                    Ok((identity, request))
                })?;
            let request_id = request.request_id.clone();
            let client = st2::resource_observe::submit_request(&root, &host, &request)?;
            let waited = client.wait_for_terminal(Duration::from_secs(wait))?;
            if waited.timed_out {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "status": "timeout",
                            "requestId": request_id,
                            "queued": true,
                            "receipt": &waited.receipt,
                        }))?
                    );
                }
                let last = waited
                    .receipt
                    .as_ref()
                    .map(|receipt| receipt.status.as_str())
                    .unwrap_or("pending");
                anyhow::bail!(
                    "resource refresh for {identity} {name} exceeded the {wait}s client wait bound \
                     (last status: {last}); the request remains queued and may still settle"
                );
            }
            let receipt = waited
                .receipt
                .context("Resource observation ended without a receipt")?;
            if json {
                println!("{}", serde_json::to_string(&receipt)?);
            }
            if !receipt.status.is_success() {
                anyhow::bail!(
                    "resource refresh for {identity} {name}: {}{}",
                    receipt.status.as_str(),
                    receipt
                        .diagnostic
                        .as_deref()
                        .map(|detail| format!(": {detail}"))
                        .unwrap_or_default()
                );
            }
            if !json {
                let digest = receipt
                    .digest
                    .map(|digest| format!(" ({digest})"))
                    .unwrap_or_default();
                println!(
                    "resource {name} on {identity}: {}{digest}",
                    receipt.status.as_str()
                );
            }
            Ok(())
        }
        ResourceCmd::Add {
            name,
            uri,
            reason,
            inactive_reason,
            selector_json,
            agent,
            agent_id,
            json,
            ctx,
        } => {
            let selector = selector_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| anyhow::anyhow!("--selector-json is not valid JSON: {error}"))?;
            let (root, host, actor, target) = resource_author_target(agent, agent_id, &ctx)?;
            let receipt = st2::agent_author::add_resource_with_selector(
                &root,
                &target,
                &host,
                actor.as_deref(),
                &name,
                &uri,
                &reason,
                inactive_reason.as_deref(),
                selector.as_ref(),
            )?;
            if json {
                println!("{}", serde_json::to_string(&receipt)?);
            } else {
                println!(
                    "{:?} resource {} on {}",
                    receipt.result, receipt.name, receipt.identity
                );
            }
            Ok(())
        }
        ResourceCmd::Remove {
            name,
            agent,
            agent_id,
            json,
            ctx,
        } => {
            let (root, host, actor, target) = resource_author_target(agent, agent_id, &ctx)?;
            let receipt =
                st2::agent_author::remove_resource(&root, &target, &host, actor.as_deref(), &name)?;
            if json {
                println!("{}", serde_json::to_string(&receipt)?);
            } else {
                println!(
                    "{:?} resource {} on {}",
                    receipt.result, receipt.name, receipt.identity
                );
            }
            Ok(())
        }
        ResourceCmd::Rename {
            old,
            new,
            agent,
            agent_id,
            json,
            ctx,
        } => {
            let (root, host, actor, target) = resource_author_target(agent, agent_id, &ctx)?;
            let receipt = st2::agent_author::rename_resource(
                &root,
                &target,
                &host,
                actor.as_deref(),
                &old,
                &new,
            )?;
            if json {
                println!("{}", serde_json::to_string(&receipt)?);
            } else {
                println!(
                    "{:?} resource {} -> {} on {}",
                    receipt.result, receipt.old, receipt.new, receipt.identity
                );
            }
            Ok(())
        }
    }
}

/// The catalog root, host, acting actor, and authored target for one mediated binding edit.
fn resource_author_target(
    agent: Option<String>,
    agent_id: Option<String>,
    ctx: &MsgCtx,
) -> Result<(PathBuf, String, Option<String>, String)> {
    let (root, host) = resolve_ctx(ctx)?;
    let actor = ctx
        .as_id
        .clone()
        .or_else(|| std::env::var("ST_AGENT").ok())
        .filter(|value| !value.is_empty());
    let target = match selected_declaration(&root, &host, agent, agent_id)? {
        Some(target) => target,
        None => actor.clone().context(
            "no resource binding target: pass --agent, --agent-id, --as, or set $ST_AGENT",
        )?,
    };
    Ok((root, host, actor, target))
}

/// Resolve an agent's context dir (`<agent_dir>/resources/context`). Identity defaults to `$ST_AGENT`.
fn resolve_context_dir(
    identity: Option<String>,
    agent_id: Option<String>,
    ctx: &MsgCtx,
) -> Result<PathBuf> {
    let (root, host) = resolve_ctx(ctx)?;
    let id = match selected_route(&root, &host, identity, agent_id)? {
        Some(identity) => identity,
        None => acting_route(&root, &host, ctx)?,
    };
    Ok(st2::context::context_dir(&agent_dir_of(&root, &id, &host)?))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `st2 up <single-file-spec>` — supervise the spec's top-level team as a fleet: keep-alive + respawn,
/// nomad-decoupled, exactly like `st2 up --catalog <catalog>`. `--once` does a
/// single boot pass; otherwise it supervises on a timer under a host lock until Ctrl-C.
fn up_spec_fleet(spec_file: &Path, host: Option<String>, once: bool, interval: u64) -> Result<()> {
    let (spec, root) = st2::eval_run::load_spec(spec_file)?;
    // File-level taxonomy (the maintainer): a file with NO agents is an eval-only ("job") file — it has nothing
    // to supervise; it is run to a verdict via `st2 eval`, not `st2 up`. Refuse it here with a pointer.
    if spec.agents.is_empty() {
        anyhow::bail!(
            "{} declares no agents — it is an eval-only file (run it with `st2 eval {}`), not a fleet to \
             supervise with `st2 up`",
            spec_file.display(),
            spec_file.display()
        );
    }
    // Run host: --host (explicit) › the spec's top-level `host` › the OS hostname. The declared host
    // wins over the OS hostname so a per-host file ups its own slice even when they differ.
    let this_host = host
        .or_else(|| spec.host.clone())
        .unwrap_or_else(detect_host);
    // Seats boot as fresh top-level agents. Authored bare `st2` commands use the PATH prepend while
    // generated DING sidecars bind directly to the executable captured by this context.
    let task_context = st2::reconcile::TaskCompileContext::current(root.clone())?;
    st2::eval_run::prepare_spawn_env(task_context.st2_executable());
    let mut specs = st2::eval_run::spec_to_agent_specs(&spec.agents, &this_host, &root);
    st2::reconcile::compile_generated_tasks(&mut specs, &this_host, &task_context)?;
    let runner = SystemRunner::new(root.clone(), exec_state_dir(&this_host));

    // One supervisor per (spec dir, host) — the same host-lock discipline as the catalog path.
    let lock = HostLock::new(&root, &this_host);
    if let Some(owner) = lock.live_owner() {
        eprintln!("st2: {}", lock.busy_warning(owner));
        std::process::exit(1);
    }

    if once {
        let report = st2::up_once_specs(&specs, &this_host, &runner);
        println!(
            "booted team from spec {} on host '{this_host}' (once):",
            spec_file.display()
        );
        print_report(&report);
        if report.skipped {
            anyhow::bail!("one-shot reconcile pass was skipped");
        }
        if !report.errors.is_empty() {
            anyhow::bail!("one-shot reconcile pass reported errors");
        }
        return Ok(());
    }

    if lock.has_stale_lock() {
        eprintln!("st2: reclaiming a stale lock left by a crashed st2.");
    }
    lock.acquire().context("acquiring host lock")?;
    eprintln!(
        "st2: supervising spec {} on host '{this_host}' ({} agents; reconcile every {interval}s; Ctrl-C to stop)",
        spec_file.display(),
        specs.len()
    );
    let result = st2::up_loop_specs(
        &specs,
        &root,
        &this_host,
        &runner,
        Duration::from_secs(interval),
        |report| {
            if report.is_noteworthy() {
                print_report(report);
            }
        },
    );
    lock.release();
    result
}

fn up(
    root: &Path,
    host: Option<String>,
    once: bool,
    materialize_only: bool,
    interval: u64,
    agent: Option<String>,
    task: Option<String>,
) -> Result<()> {
    // An st2-SPEC path (a `*.kdl` file, or a folder with one top-level spec `*.kdl`) supervises its
    // top-level team directly — no catalog discovery. Otherwise, the classic catalog reconcile loop.
    if let Some(spec_file) = st2::eval_run::resolve_spec_path(root) {
        if task.is_some() {
            anyhow::bail!("--task is for folder catalogs, not single-file specs");
        }
        if materialize_only {
            anyhow::bail!(
                "--materialize-only is for folder catalogs with agent render{{}} blocks, not single-file specs"
            );
        }
        return up_spec_fleet(&spec_file, host, once, interval);
    }
    let this_host = host.unwrap_or_else(detect_host);
    // Canonicalize the catalog root so `$CATALOG` expands to an absolute path (T01/R11).
    let catalog_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    if materialize_only {
        let _catalog_lock = st2::CatalogLock::shared(&catalog_root)
            .context("acquire shared catalog-authoring lock for materialization")?;
        let mut found = discover(&catalog_root);
        let ownership_specs = found.specs.clone();
        if let Some(selector) = task.as_deref() {
            let (owner, _, _) = st2::reconcile::resolve_task(&found.specs, selector, &this_host)?;
            let owner_identity = owner.identity.clone();
            let owner_path = owner.path.clone();
            found
                .specs
                .retain(|spec| spec.identity == owner_identity && spec.path == owner_path);
        }
        if let Some(identity) = agent.as_deref() {
            found
                .specs
                .retain(|spec| spec.identity == identity || spec.bus_id(&this_host) == identity);
        }
        for warning in &found.warnings {
            eprintln!("warning: {warning}");
        }
        for error in &found.errors {
            eprintln!("error: {}: {}", error.path.display(), error.message);
        }
        if st2::hooks::required_by_codex(&found.specs, &this_host, &catalog_root) {
            st2::hooks::verify_required_set().context(
                "verifying explicitly installed lifecycle hooks before Codex materialization",
            )?;
        }
        let report = st2::materialize::materialize_catalog_against(
            &catalog_root,
            &found.specs,
            &ownership_specs,
            &this_host,
        );
        for item in &report.materialized {
            println!("{item}");
        }
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
        for error in &report.errors {
            eprintln!("error: {error}");
        }
        println!(
            "materialized {} operation{} for host '{this_host}'; {} error{}",
            report.materialized.len(),
            plural(report.materialized.len()),
            found.errors.len() + report.errors.len(),
            plural(found.errors.len() + report.errors.len())
        );
        if !found.errors.is_empty() || !report.is_clean() {
            anyhow::bail!("materialization failed");
        }
        return Ok(());
    }

    let runner = SystemRunner::new(catalog_root.clone(), exec_state_dir(&this_host));

    // One supervisor per (folder, host). A single `--once` pass must also refuse while a loop owns
    // the lock (it would double-spawn) — but it does NOT take the lock itself (that would clobber the
    // loop's pid file); only the loop acquires + holds it.
    let lock = HostLock::new(root, &this_host);
    if let Some(owner) = lock.live_owner() {
        eprintln!("st2: {}", lock.busy_warning(owner));
        std::process::exit(1);
    }

    if once {
        let targeted = task.is_some();
        let report = match task.as_deref() {
            Some(selector) => {
                st2::run::up_once_selected(&catalog_root, selector, &this_host, &runner)?
            }
            None => up_once(&catalog_root, &this_host, &runner)?,
        };
        println!("reconcile pass on host '{this_host}':");
        print_report(&report);
        if report.skipped {
            anyhow::bail!("one-shot reconcile pass was skipped");
        }
        if targeted && !report.errors.is_empty() {
            anyhow::bail!("targeted one-shot reconcile pass reported errors");
        }
        if !report.errors.is_empty() {
            anyhow::bail!("one-shot reconcile pass reported errors");
        }
        return Ok(());
    }

    if lock.has_stale_lock() {
        eprintln!("st2: reclaiming a stale lock left by a crashed st2.");
    }
    lock.acquire().context("acquiring host lock")?;

    eprintln!(
        "st2: supervising {} on host '{this_host}' (reconcile every {interval}s + on change; Ctrl-C to stop)",
        root.display()
    );
    let result = up_loop(
        &catalog_root,
        &this_host,
        &runner,
        Duration::from_secs(interval),
        |report| {
            if report.is_noteworthy() {
                print_report(report);
            }
        },
    );
    lock.release();
    result
}

fn print_report(report: &UpReport) {
    report_line("launched", &report.launched);
    report_line("restarted", &report.restarted);
    report_line("torn down", &report.torn_down);
    report_line("gc", &report.gc);
    report_line("held", &report.held);
    report_line("flapping", &report.flapping);
    report_line("unparked", &report.unparked);
    report_line("adopted", &report.adopted);
    report_line("other-host", &report.other_host);
    report_line("unrunnable", &report.unrunnable);
    report_line("archived", &report.archived);
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    for e in &report.errors {
        eprintln!("error: {e}");
    }
}

fn report_line(label: &str, items: &[String]) {
    if !items.is_empty() {
        println!("  {label} ({}): {}", items.len(), items.join(", "));
    }
}

fn ls(root: &Path) -> Result<()> {
    // A single-file st2 spec: PREVIEW the declared team WITHOUT booting (a safe validate/inspect path
    // before `st2 up`). A catalog dir has no top-level spec .kdl, so it falls through to discovery.
    if let Some(spec_file) = st2::eval_run::resolve_spec_path(root) {
        let (spec, _dir) = st2::eval_run::load_spec(&spec_file)?;
        // File-level taxonomy: no agents = an eval-only file → `st2 eval`; agents = a fleet → `st2 up`.
        let classification = if spec.agents.is_empty() {
            "eval-only → st2 eval"
        } else {
            "agents → st2 up"
        };
        println!(
            "spec {}  [{classification}]  (host {}; {} agent{}{})",
            spec_file.display(),
            spec.host.as_deref().unwrap_or("<none>"),
            spec.agents.len(),
            if spec.agents.len() == 1 { "" } else { "s" },
            if spec.eval.is_some() {
                ", + eval block"
            } else {
                ""
            },
        );
        for a in &spec.agents {
            println!(
                "  {}  ws={}",
                a.id,
                a.workspace.as_deref().unwrap_or("<none>")
            );
            println!("      command: {}", a.command);
            for ex in &a.execs {
                println!("      + exec {}: {}", ex.id, ex.command);
            }
        }
        return Ok(());
    }
    let _catalog_lock = st2::CatalogLock::shared(root)
        .context("acquire shared catalog-authoring lock for catalog listing")?;
    let found = discover(root);
    let mut specs = found.specs.clone();
    let task_context = st2::reconcile::TaskCompileContext::current(root.to_path_buf())?;
    st2::reconcile::compile_generated_tasks(&mut specs, &detect_host(), &task_context)
        .context("compile generated tasks for catalog listing")?;

    if specs.is_empty() {
        println!("no specs found under {}", root.display());
    }
    for spec in &specs {
        let host = spec.host.as_deref().unwrap_or("<this-host>");
        let kind = match spec.job_type {
            st2::JobType::Service => "service",
        };
        let runnable = if spec.is_runnable() {
            ""
        } else {
            "  [UNRENDERED: no task launch]"
        };
        let lifecycle = if spec.desired_state.is_running() {
            String::new()
        } else {
            format!(
                "  [{}{}]",
                spec.desired_state.as_str(),
                spec.desired_state
                    .reason()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            )
        };
        println!(
            "{host}.{ident}  [{kind}] ({n} task{plural}){runnable}{lifecycle}\n    {path}",
            ident = spec.identity,
            n = spec.tasks.len(),
            plural = if spec.tasks.len() == 1 { "" } else { "s" },
            path = spec.path.display(),
        );
        for task in &spec.tasks {
            let tk = match task.kind {
                st2::TaskKind::Pty => "pty",
                st2::TaskKind::Exec => "exec",
            };
            let launch = match (&task.command, &task.argv) {
                (Some(command), _) => format!("command {command}"),
                (_, Some(argv)) => format!("argv {argv:?}"),
                _ => "<none>".to_string(),
            };
            println!("      - {tk} {name}: {launch}", name = task.name);
        }
    }

    for w in &found.warnings {
        eprintln!("warning: {w}");
    }
    for e in &found.errors {
        eprintln!("error: {}: {}", e.path.display(), e.message);
    }

    Ok(())
}
