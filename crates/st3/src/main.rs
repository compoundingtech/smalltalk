use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use clap::{Args, CommandFactory as _, Parser, Subcommand, ValueEnum};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use st3::api::{AppState, fabric_router, router, serve_unix};
use st3::client::{Client, Endpoint};
use st3::config::{Config, PeerConfig};
use st3::model::{
    ApplyRequest, ApplyResponse, AttachRequest, Attachment, AttentionItemView, AttentionRequest,
    AttentionRequestView, AttentionResolveRequest, AttentionWithdrawRequest, ClaimInput,
    ClaimRecord, ClaimsPage, CurrentHarnessView, DoctorReport, DocumentListResponse,
    DocumentPutRequest, DocumentVersion, EvalStatus, EventRecord, IntentInput,
    LaunchApproveAndStartRequest, LaunchApproveAndStartView, LaunchDecisionAnswerRequest,
    LaunchDecisionOption, LaunchDecisionRequest, LaunchDecisionResponse, LaunchDecisionType,
    LaunchStartRequest, MessageLifecycleRequest, MessagePage, MessageSendRequest, MessageView,
    MissionOutputView, MissionProductionRequest, MissionRequest, MissionResponse,
    MissionRevisionRequest, MissionRunView, MissionState, OperationalRepairApplyRequest,
    OperationalRepairPlan, OperationalRepairResult, PlannerSpec, PlanningApprovalRequest,
    PlanningCandidateSubmitRequest, PlanningProposalRequest, PlanningSessionView,
    ReplicaRecordView, ReplicationPeerStatus, ReplicationRepairRequest, ReplicationStatus,
    ReviewRequest, RevisionApprovalRequest, RevisionCancelRequest, RevisionProposalView,
    RevisionSubmissionView, RunGenerationView, SessionControlResponse, SessionInputMode,
    SessionInputRequest, SessionScreen, SessionSignalRequest, StatusResponse, StepRunView,
    SubscriptionRequestDecision, SubscriptionRequestView, WorkRequest, WorkRetryRequest,
    WorkWakeRequest,
};
use st3::reconcile::Reconciler;
use st3::store::Store;
use st3_client::{
    API_VERSION as CLIENT_V0_API_VERSION, Client as GeneratedClient,
    ClientError as GeneratedClientError, Envelope as ClientEnvelope, ErrorCode as ClientErrorCode,
    EventPage as ClientEventPage, EventType as ClientEventType, Fence as ClientFence,
    Page as ClientPage, PairingBegin, Resource as ClientResource,
    TargetParameters as ClientTargetParameters, TerminalInputMode as ClientTerminalInputMode,
    TerminalInputParameters as ClientTerminalInputParameters,
    TerminalScreen as ClientTerminalScreen, TimelineBody as ClientTimelineBody,
    TimelineEntry as ClientTimelineEntry, TimelinePage as ClientTimelinePage, catch_up_estimate,
    envelope_count,
};
use tokio::sync::{Notify, watch};

mod presentation;

use presentation::{
    OutputStyle, follow_snapshot, glance, mission_run_signature, relative_time,
    render_attention_show, render_generation, render_generations, render_human_value,
    render_mission_run, render_revision_proposal, render_step_run, shell_argument,
};

#[derive(Parser)]
#[command(
    name = "st",
    bin_name = "st",
    version,
    about = "Coordinate durable agent work across machines without losing operational truth"
)]
struct Cli {
    #[arg(long, global = true)]
    endpoint: Option<String>,
    #[arg(long, global = true, hide = true)]
    catalog: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    /// Keep retrying for this many seconds while the st daemon is unreachable, for example while
    /// it restarts during a deploy. 0 fails at once.
    #[arg(
        long,
        global = true,
        env = "ST3_DAEMON_WAIT",
        value_name = "SECONDS",
        default_value_t = DEFAULT_DAEMON_WAIT_SECS
    )]
    daemon_wait: u64,
    #[command(subcommand)]
    command: Command,
}

/// A deploy restarts the daemon in seconds; a CLI call made meanwhile waits it out instead of
/// failing an agent's step.
const DEFAULT_DAEMON_WAIT_SECS: u64 = 30;

#[derive(Subcommand)]
enum Command {
    /// Start the HTTP API, readers, peers, and reconciler.
    Up(UpArgs),
    /// Understand what needs action now.
    Now(NowArgs),
    /// Inspect and control missions.
    Missions {
        #[command(subcommand)]
        command: MissionViewCommand,
    },
    /// Create and review a durable planner-backed launch.
    Launch {
        #[command(subcommand)]
        command: LaunchCommand,
    },
    /// Show and manage work that needs a person.
    Attention {
        #[command(subcommand)]
        command: AttentionCommand,
    },
    /// Assess fleet machines, health, and capacity.
    Machines(MachinesArgs),
    /// Inspect current agents or explicit agent history.
    Agents {
        #[command(subcommand)]
        command: AgentsCommand,
    },
    /// Read, follow, and send normalized conversations.
    Conversations {
        #[command(subcommand)]
        command: MessageCommand,
    },
    /// Read bounded changes since a stable cursor.
    Activity(ActivityArgs),
    /// Pair, inspect, and revoke client devices.
    Devices(DevicesArgs),
    /// Claim and update durable mission work.
    Work {
        #[command(subcommand)]
        command: WorkCommand,
    },
    /// Inspect and control terminal members.
    Terminals {
        #[command(subcommand)]
        command: PtyCommand,
    },
    /// Check the daemon and runtime dependencies.
    Doctor(DoctorArgs),
    /// Preview or apply bounded graph-authorized operational repairs.
    Repair {
        #[command(subcommand)]
        command: RepairCommand,
    },
    /// Remove st3 from this machine: leave its fleet, then its services, state, and settings.
    Uninstall(UninstallArgs),
    /// Join machines into a fleet: create, invite, join, and inspect members.
    Fleet {
        #[command(subcommand)]
        command: FleetCommand,
    },
    /// Inspect and repair fleet replication.
    Replication {
        #[command(subcommand)]
        command: ReplicationCommand,
    },
    /// Manage the Linux or macOS st user service.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Manage the ST Claude Code channel plugin and approval policy.
    #[command(hide = true)]
    ClaudeChannel {
        #[command(subcommand)]
        command: ClaudeChannelCommand,
    },
    /// Inspect a typed subject card or its bounded history.
    Subject {
        #[command(subcommand)]
        command: SubjectCommand,
    },
    /// Publish one registered typed observation.
    Claim(ClaimArgs),
    /// Report a harness failure as this agent through the authorized diagnostic path.
    Diagnostic(HarnessDiagnosticArgs),
    /// Trace bounded graph history or wait on a graph condition.
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
    /// Inspect the authoritative subject, resource, and claim schema.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Store or read immutable documents.
    Documents {
        #[command(subcommand)]
        command: DocCommand,
    },
    /// Discover native harness sessions and move one under durable st ownership.
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    /// Generate one shell completion script.
    Completions(CompletionsArgs),
    #[command(hide = true)]
    ReplicationWorker(ReplicationWorkerArgs),
    #[command(hide = true)]
    Driver(DriverArgs),
}

#[derive(Args)]
struct ReplicationWorkerArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    node: Option<String>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    peer_listen: Option<String>,
    #[arg(long)]
    fleet_id: Option<String>,
    #[arg(long)]
    shared_secret_file: Option<PathBuf>,
    #[arg(long, value_parser = parse_peer)]
    peer: Vec<PeerConfig>,
}

#[derive(Subcommand)]
enum FleetCommand {
    /// Found a new fleet with this machine as its first member (the anchor).
    Create(FleetCreateArgs),
    /// Create a single-use code that lets one new machine join.
    Invite(FleetInviteArgs),
    /// List invites, or revoke one.
    Invites {
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        command: Option<FleetInvitesCommand>,
    },
    /// Join this machine to a fleet with a code from `st fleet invite`.
    Join(FleetJoinArgs),
    /// Remove another member, or a config peer, from the fleet.
    Remove(FleetRemoveArgs),
    /// Take this machine out of its fleet after everything it wrote has reached a member.
    Leave(FleetLeaveArgs),
    /// Move a machine of a config-peer fleet to membership, keeping its history.
    Migrate(FleetMigrateArgs),
    /// Switch this member between listening and dial-out.
    Mode(FleetModeArgs),
    /// Show this node, the fleet's members, and open invites.
    Status,
}

#[derive(Subcommand)]
enum FleetInvitesCommand {
    /// Revoke an invite. The sponsor refuses it once the revocation reaches it.
    Revoke {
        invite: String,
        #[arg(long)]
        reason: String,
        #[arg(long = "as")]
        actor: Option<String>,
    },
}

#[derive(Args, Clone)]
struct FleetMemberArgs {
    /// Accept no connections and dial listening members: for a laptop that is often away.
    #[arg(long)]
    dial_out: bool,
    /// The replication port. The default is 31313 or the next free port.
    #[arg(long)]
    port: Option<u16>,
    /// Transports to listen on and dial with: tailscale, fabric. The default detects both.
    #[arg(long, value_delimiter = ',')]
    transports: Option<Vec<String>>,
    /// Also announce the loopback endpoint (for nodes on one machine and for tunnels).
    #[arg(long)]
    advertise_loopback: bool,
    #[arg(long, hide = true)]
    fabric: Option<PathBuf>,
    #[arg(long, hide = true)]
    tailscale: Option<PathBuf>,
}

impl FleetMemberArgs {
    fn settings(&self) -> st3::fleet::join::MemberSettings {
        st3::fleet::join::MemberSettings {
            mode: if self.dial_out {
                st3::config::FleetMode::DialOut
            } else {
                st3::config::FleetMode::Listening
            },
            port: self.port,
            transports: self.transports.clone(),
            advertise_loopback: self.advertise_loopback,
            fabric: self.fabric.clone(),
            tailscale: self.tailscale.clone(),
        }
    }
}

#[derive(Args)]
struct FleetCreateArgs {
    /// This machine's name in the fleet. The default is the configured node name.
    #[arg(long)]
    name: Option<String>,
    /// Do not install or restart services; print the foreground commands instead.
    #[arg(long)]
    no_service: bool,
    #[command(flatten)]
    member: FleetMemberArgs,
}

#[derive(Args)]
struct FleetInviteArgs {
    /// The name the new machine must use.
    name: Option<String>,
    /// How long the code stays valid: 10s to 24h.
    #[arg(long, default_value = "15m")]
    expires: String,
    /// Which of this member's endpoints go into the code: auto, tailscale, fabric, loopback.
    #[arg(long, default_value = "auto")]
    via: String,
    /// A code that moves an existing config-peer machine to membership.
    #[arg(long)]
    migrate: bool,
    /// Send the code to NAME's Fabric inbox instead of showing it.
    #[arg(long)]
    send_fabric: bool,
    /// Print only the code.
    #[arg(long)]
    code_only: bool,
    /// Write the code to a new 0600 file instead of printing it.
    #[arg(long)]
    code_file: Option<PathBuf>,
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct FleetJoinArgs {
    /// The join code, or - to read it from standard input. Without it, join asks for it.
    code: Option<String>,
    /// Read the code from this file, and delete the file after a successful join.
    #[arg(long)]
    code_file: Option<PathBuf>,
    /// Read the code that `st fleet invite --send-fabric` put in this machine's Fabric inbox.
    #[arg(long)]
    fabric_inbox: bool,
    /// This machine's name in the fleet.
    #[arg(long)]
    name: Option<String>,
    /// A loopback URL that reaches the sponsor, instead of the code's endpoints.
    #[arg(long)]
    via: Option<String>,
    /// Do not stop, install, or start services; print the foreground commands instead.
    #[arg(long)]
    no_service: bool,
    #[command(flatten)]
    member: FleetMemberArgs,
}

fn parse_fleet_duration(text: &str) -> Result<u64> {
    let text = text.trim();
    let (number, unit) = text
        .find(|character: char| !character.is_ascii_digit())
        .map_or((text, "s"), |split| text.split_at(split));
    let number: u64 = number
        .parse()
        .context("a duration is a number and a unit, like 15m")?;
    Ok(number
        * match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            _ => anyhow::bail!("a duration unit is s, m, or h"),
        })
}

/// The person a fleet command acts for. Inside an agent seat it must be explicit.
fn fleet_person(actor: Option<String>, config: &Config) -> Result<String> {
    if let Some(actor) = actor {
        return Ok(actor);
    }
    anyhow::ensure!(
        std::env::var("ST_AGENT").is_err(),
        "inside an agent seat, fleet commands need an explicit --as person/NAME"
    );
    config
        .person
        .clone()
        .context("set person in config.toml or pass --as person/NAME")
}

fn read_code_without_echo() -> Result<String> {
    use std::io::{BufRead as _, IsTerminal as _};
    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let mut saved = None;
    if terminal {
        eprint!("Paste the join code: ");
        // SAFETY: plain termios calls on standard input; the saved settings are restored below.
        unsafe {
            let mut settings: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut settings) == 0 {
                saved = Some(settings);
                settings.c_lflag &= !libc::ECHO;
                libc::tcsetattr(0, libc::TCSANOW, &settings);
            }
        }
    }
    let mut line = String::new();
    let result = stdin.lock().read_line(&mut line);
    if let Some(settings) = saved {
        // SAFETY: restores the settings read above.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &settings);
        }
        eprintln!();
    }
    result?;
    Ok(line.trim().to_owned())
}

/// The one `st-fleet-join-*.code` file in this machine's Fabric inbox.
fn fabric_inbox_code() -> Result<PathBuf> {
    let home = std::env::var_os("FABRIC_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/fabric"))
        })
        .context("HOME is not set")?;
    let inbox = home.join("inbox");
    let mut found = Vec::new();
    for sender in fs::read_dir(&inbox)
        .with_context(|| format!("read the Fabric inbox {}", inbox.display()))?
    {
        let sender = sender?.path();
        if !sender.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&sender)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("st-fleet-join-") && name.ends_with(".code"))
            {
                found.push(path);
            }
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => anyhow::bail!("no join code in the Fabric inbox {}", inbox.display()),
        _ => anyhow::bail!(
            "{} join codes in the Fabric inbox {}; remove the old ones",
            found.len(),
            inbox.display()
        ),
    }
}

fn services_installed() -> bool {
    st3::service::status()
        .map(|report| report.services.iter().any(|service| service.installed))
        .unwrap_or(false)
}

async fn run_fleet(endpoint: &Endpoint, command: FleetCommand, json_output: bool) -> Result<()> {
    let config = Config::load_unvalidated(None)?;
    let client = Client::new(endpoint.clone());
    match command {
        FleetCommand::Create(args) => {
            anyhow::ensure!(
                config.fleet_id.is_none(),
                "config.toml already configures a fleet with config peers; move it to membership with st fleet migrate"
            );
            let node = args.name.unwrap_or_else(|| config.node.clone());
            let founded =
                st3::fleet::join::found(&config.state_dir, &node, &args.member.settings())?;
            if json_output {
                return print_value(&founded, true);
            }
            println!(
                "Created fleet {} with {} as its first member.",
                founded.fleet_id, founded.node
            );
            if !args.no_service && services_installed() {
                st3::service::install(Config::load_with_fleet(None)?)?;
                println!("The st3 services now run as a fleet member. Next: st fleet invite NAME");
            } else {
                println!(
                    "Restart st3 up and start st3 replication-worker, then: st fleet invite NAME"
                );
            }
            Ok(())
        }
        FleetCommand::Invite(args) => {
            let person = fleet_person(args.actor.clone(), &config)?;
            if args.send_fabric {
                anyhow::ensure!(
                    args.name.is_some(),
                    "--send-fabric needs the NAME of the machine to send it to"
                );
            }
            let created: st3::api::FleetInviteCreated = client
                .post(
                    "/v1/internal/fleet/invites",
                    &st3::api::FleetInviteRequest {
                        name: args.name.clone(),
                        expires_seconds: parse_fleet_duration(&args.expires)?,
                        via: Some(args.via.clone()),
                        migrate: args.migrate,
                        person,
                    },
                )
                .await?;
            let command = if args.migrate { "migrate" } else { "join" };
            if let Some(path) = &args.code_file {
                st3::fleet::join::write_private(path, created.code.as_bytes())?;
                println!("{}\t{}", created.invite, path.display());
            } else if args.send_fabric {
                let name = args.name.as_deref().unwrap_or_default();
                let fabric = st3::config::FleetFile::load(&config.state_dir)?
                    .and_then(|file| file.fabric)
                    .or_else(|| st3::fleet::transport::resolve_tool(None, "fabric"))
                    .context("--send-fabric needs the fabric command")?;
                let temporary = config
                    .state_dir
                    .join("fleet")
                    .join(format!(".send-{}.code", std::process::id()));
                st3::fleet::join::write_private(&temporary, created.code.as_bytes())?;
                let invite_id = created.invite.trim_start_matches("fleet-invite/");
                let sent = std::process::Command::new(&fabric)
                    .args(["send-file", name])
                    .arg(&temporary)
                    .args(["--as", &format!("st-fleet-join-{invite_id}.code")])
                    .status();
                let _ = fs::remove_file(&temporary);
                anyhow::ensure!(
                    sent?.success(),
                    "fabric send-file to {name} failed; the invite stays open until it expires"
                );
                println!(
                    "Sent invite {} to {name}'s Fabric inbox. On {name}, run:\n  st fleet {command} --fabric-inbox",
                    created.invite
                );
            } else if args.code_only {
                println!("{}", created.code);
            } else if json_output {
                return print_value(&created, true);
            } else {
                let expires_in =
                    (u128::from(created.expires_at_unix_ms).saturating_sub(unix_ms()) / 60_000) + 1;
                let target = args.name.as_deref().unwrap_or("the new machine");
                println!(
                    "Invite {} for {target} expires in about {expires_in} minutes.",
                    created.invite
                );
                println!(
                    "On {target}, run this and paste the code when asked:\n  st fleet {command}"
                );
                println!("Code:\n  {}", created.code);
                if let Some(name) = &args.name {
                    println!(
                        "Or, if {name} is a Fabric peer of this machine, send the code instead of showing it:\n  st fleet invite {name} --send-fabric\n  fabric exec {name} -- st fleet {command} --fabric-inbox"
                    );
                }
            }
            Ok(())
        }
        FleetCommand::Invites { all, command } => match command {
            None => {
                let invites: Vec<st3::store::FleetInviteView> = client
                    .get(&format!("/v1/internal/fleet/invites?all={all}"))
                    .await?;
                if json_output {
                    return print_value(&invites, true);
                }
                println!("INVITES  {}", invites.len());
                for invite in invites {
                    let detail = match invite.state.as_str() {
                        "redeemed" => format!(
                            "by {} (key {}…)",
                            invite.redeemed_name.as_deref().unwrap_or("?"),
                            invite
                                .redeemed_key
                                .as_deref()
                                .map(|key| &key[..key.len().min(8)])
                                .unwrap_or("?")
                        ),
                        "revoked" => invite.revoked_reason.clone().unwrap_or_default(),
                        _ => format!("for {}", invite.name.as_deref().unwrap_or("any name")),
                    };
                    println!(
                        "{}  {}  sponsor {}  {}",
                        invite.invite, invite.state, invite.sponsor, detail
                    );
                }
                Ok(())
            }
            Some(FleetInvitesCommand::Revoke {
                invite,
                reason,
                actor,
            }) => {
                let person = fleet_person(actor, &config)?;
                let _: Value = client
                    .post(
                        "/v1/internal/fleet/invites/revoke",
                        &st3::api::FleetInviteRevokeRequest {
                            invite: invite.clone(),
                            reason,
                            person,
                        },
                    )
                    .await?;
                println!("revoked\t{invite}");
                Ok(())
            }
        },
        FleetCommand::Join(args) => {
            let (code, code_path) = if args.fabric_inbox {
                let path = fabric_inbox_code()?;
                (fs::read_to_string(&path)?, Some(path))
            } else if let Some(path) = &args.code_file {
                (fs::read_to_string(path)?, Some(path.clone()))
            } else {
                match args.code.as_deref() {
                    Some("-") => {
                        let mut text = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                        (text, None)
                    }
                    Some(code) => (code.to_owned(), None),
                    None => (read_code_without_echo()?, None),
                }
            };
            let use_services = !args.no_service && services_installed();
            if client.get::<Value>("/v1/health").await.is_ok() {
                anyhow::ensure!(
                    use_services,
                    "stop the running st3 daemon first: nothing may write while this machine joins"
                );
                st3::service::stop()?;
            }
            let joined = st3::fleet::join::join(&st3::fleet::join::JoinOptions {
                state_dir: config.state_dir.clone(),
                configured_node: config.node.clone(),
                code: code.trim().to_owned(),
                name: args.name.clone(),
                via: args.via.clone(),
                settings: args.member.settings(),
                legacy_secret_file: None,
                fabric_protocol: None,
            })
            .await?;
            if let Some(path) = code_path {
                let _ = fs::remove_file(path);
            }
            if json_output {
                print_value(&joined, true)?;
            } else {
                println!(
                    "{} joined fleet {} through {}{}.",
                    joined.name,
                    joined.fleet_id,
                    joined.sponsor,
                    if joined.resumed { " (resumed)" } else { "" }
                );
            }
            if use_services {
                st3::service::install(Config::load_with_fleet(None)?)?;
                println!(
                    "The st3 services now run as a fleet member; st fleet status shows the sync."
                );
            } else if !json_output {
                println!("Start st3 up and st3 replication-worker to begin syncing.");
            }
            Ok(())
        }
        FleetCommand::Remove(args) => run_fleet_remove(&client, &config, args).await,
        FleetCommand::Migrate(args) => run_fleet_migrate(&client, &config, args).await,
        FleetCommand::Mode(args) => {
            let mut file = st3::config::FleetFile::load(&config.state_dir)?
                .context("this machine is not a fleet member")?;
            file.mode = match args.mode.as_str() {
                "listening" => st3::config::FleetMode::Listening,
                "dial-out" => st3::config::FleetMode::DialOut,
                _ => anyhow::bail!("the mode is listening or dial-out"),
            };
            file.port = match file.mode {
                st3::config::FleetMode::DialOut => None,
                st3::config::FleetMode::Listening => Some(match args.port.or(file.port) {
                    Some(port) => port,
                    None => st3::fleet::join::free_port(st3::fleet::join::DEFAULT_PORT)?,
                }),
            };
            file.save(&config.state_dir)?;
            println!(
                "This member is now {}; it announces the change when its replication worker starts.",
                file.mode.as_str()
            );
            if !args.no_service && services_installed() {
                st3::service::install(Config::load_with_fleet(None)?)?;
            } else {
                println!("Restart st3 replication-worker for the change to take effect.");
            }
            Ok(())
        }
        FleetCommand::Leave(args) => {
            let person = fleet_person(args.actor.clone(), &config)?;
            if args.cancel {
                let _: Value = client
                    .post("/v1/internal/fleet/leave/cancel", &json!({}))
                    .await?;
                println!("leave cancelled");
                return Ok(());
            }
            let use_services = !args.no_service && services_installed();
            let confirmed = fleet_leave(
                &client,
                &config,
                &person,
                args.offline,
                args.force,
                parse_fleet_duration(&args.wait)?,
            )
            .await?;
            match confirmed {
                Some(member) => println!("{member} holds everything this machine wrote."),
                None => println!(
                    "Left without reaching a member. On another member run: st fleet remove {} --reason \"left offline\"",
                    config.node
                ),
            }
            if use_services {
                st3::service::install(Config::load_with_fleet(None)?)?;
                println!("st3 now runs local-only on this machine.");
            } else {
                println!(
                    "Stop st3 replication-worker; st3 up now runs local-only after a restart."
                );
            }
            Ok(())
        }
        FleetCommand::Status => {
            let status: st3::api::FleetStatus = client.get("/v1/internal/fleet/status").await?;
            if json_output {
                return print_value(&status, true);
            }
            println!(
                "FLEET  {}   this node: {}",
                status.fleet_id.as_deref().unwrap_or("none"),
                status.node
            );
            println!("MEMBER  MODE  STATE  ROUTE-ENDPOINTS");
            for member in &status.view.members {
                let transports = member
                    .endpoints
                    .iter()
                    .filter_map(|endpoint| endpoint["transport"].as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}  {}  {}{}  {}",
                    member.name,
                    member.mode,
                    member.state,
                    member
                        .ended
                        .as_deref()
                        .map(|ended| format!(" ({ended})"))
                        .unwrap_or_default(),
                    if transports.is_empty() {
                        "-".into()
                    } else {
                        transports
                    }
                );
            }
            for peer in &status.peers {
                println!(
                    "PEER  {}  {}{}",
                    peer.peer,
                    peer.status,
                    peer.last_error
                        .as_deref()
                        .map(|error| format!("  {error}"))
                        .unwrap_or_default()
                );
            }
            for invite in &status.invites {
                println!("INVITE  {}  {}", invite.invite, invite.state);
            }
            Ok(())
        }
    }
}

#[derive(Args)]
struct FleetRemoveArgs {
    /// The member (or config peer) to remove.
    name: String,
    #[arg(long)]
    reason: String,
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct FleetLeaveArgs {
    /// Leave without reaching any member; another member must then remove this one.
    #[arg(long)]
    offline: bool,
    /// Leave even while seats run on this machine.
    #[arg(long)]
    force: bool,
    /// Stop a leave that has not written its leave claim yet.
    #[arg(long)]
    cancel: bool,
    /// Do not stop, reinstall, or start services.
    #[arg(long)]
    no_service: bool,
    /// How long to wait for a member to hold everything this machine wrote.
    #[arg(long, default_value = "10m")]
    wait: String,
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct UninstallArgs {
    /// List what would be removed, and remove nothing.
    #[arg(long)]
    dry_run: bool,
    /// Remove without asking.
    #[arg(long)]
    yes: bool,
    /// Leave the fleet without reaching any member first.
    #[arg(long)]
    offline: bool,
    /// Keep the installed st3, st, stui, st3-migrate, and pty executables.
    #[arg(long)]
    keep_binaries: bool,
    /// Also required when this machine's graph exists nowhere else.
    #[arg(long)]
    erase_local_graph: bool,
    /// Never touch service managers; the daemon and worker must already be stopped.
    #[arg(long)]
    no_service: bool,
    #[arg(long = "as")]
    actor: Option<String>,
}

/// Who confirms that a member holds everything this node wrote: a peer that reports this node's
/// authority digest holds every envelope this node holds. A member that refuses this node as
/// left does so only after it admitted this node's leave, which is this node's last write. Once
/// it has, it stops exchanging with this node, so a matching digest may never come.
fn leave_confirmation(
    status: &ReplicationStatus,
    removed: Option<&st3::config::FleetRemoval>,
) -> Result<Option<String>> {
    if let Some(peer) = status
        .peers
        .iter()
        .find(|peer| peer.authority_digest.as_deref() == Some(status.authority_digest.as_str()))
    {
        return Ok(Some(peer.peer.clone()));
    }
    match removed {
        Some(removal) if removal.code == "member-left" => Ok(Some(removal.reported_by.clone())),
        Some(removal) => anyhow::bail!(
            "{} reports this machine as {}, so what it wrote after that will not replicate; \
             finish with st fleet leave --offline",
            removal.reported_by,
            removal.code
        ),
        None => Ok(None),
    }
}

/// One line per peer for a leave that timed out.
fn leave_peer_summary(status: &ReplicationStatus) -> String {
    if status.peers.is_empty() {
        return "no peers".into();
    }
    status
        .peers
        .iter()
        .map(|peer| {
            let digest = match &peer.authority_digest {
                None => "no digest reported",
                Some(digest) if *digest == status.authority_digest => "same digest",
                Some(_) => "different digest",
            };
            let error = peer
                .last_error
                .as_deref()
                .map(|error| format!(", last error: {error}"))
                .unwrap_or_default();
            format!("{} {} ({digest}{error})", peer.peer, peer.status)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Wait until a member confirms it holds everything this node wrote (see
/// `leave_confirmation`). `stage` names the wait in the error.
async fn wait_for_leave_confirmation(
    client: &Client,
    state_dir: &Path,
    stage: &str,
    seconds: u64,
) -> Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let status: ReplicationStatus = client.get("/v1/replication/status").await?;
        let file = st3::config::FleetFile::load(state_dir)?;
        let removed = file.as_ref().and_then(|file| file.removed.as_ref());
        if let Some(member) = leave_confirmation(&status, removed)? {
            return Ok(member);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "{stage}, no member reported holding everything this machine wrote within {seconds} \
             seconds ({}); run st fleet leave again, or st fleet leave --offline and remove this \
             machine from another member",
            leave_peer_summary(&status)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn running_local_runtimes(machines: &Value, node: &str) -> u64 {
    machines["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|machine| machine["host_id"] == format!("host/{node}"))
        .and_then(|machine| machine["occupancy"]["running_runtimes"].as_u64())
        .unwrap_or(0)
}

/// Leave the fleet: stop local writes, drain, write the leave as the last batch, confirm, and
/// remove this machine's fleet settings. Returns the member that confirmed, if any.
async fn fleet_leave(
    client: &Client,
    config: &Config,
    person: &str,
    offline: bool,
    force: bool,
    wait_seconds: u64,
) -> Result<Option<String>> {
    let file = st3::config::FleetFile::load(&config.state_dir)?
        .context("this machine is not a fleet member")?;
    let mut confirmed = None;
    if !offline {
        let machines: Value = client.get("/v1/client/machines").await.unwrap_or_default();
        let running = running_local_runtimes(&machines, &config.node);
        anyhow::ensure!(
            force || running == 0,
            "{running} runtimes still run on this machine; stop its seats first or pass --force"
        );
        let _: Value = client
            .post(
                "/v1/internal/fleet/leave/begin",
                &st3::api::FleetPersonRequest {
                    person: person.into(),
                },
            )
            .await?;
        wait_for_leave_confirmation(
            client,
            &config.state_dir,
            "before writing the leave",
            wait_seconds,
        )
        .await?;
        let claim: ClaimRecord = client
            .post(
                "/v1/internal/fleet/leave/claim",
                &st3::api::FleetPersonRequest {
                    person: person.into(),
                },
            )
            .await?;
        confirmed = Some(
            wait_for_leave_confirmation(
                client,
                &config.state_dir,
                "after writing the leave",
                wait_seconds,
            )
            .await?,
        );
        println!("left\t{}", claim.id);
    }
    if file
        .transports
        .iter()
        .any(|transport| transport == "fabric")
        && let Some(fabric) = st3::fleet::transport::resolve_tool(file.fabric.as_deref(), "fabric")
    {
        let protocol = file
            .fabric_protocol
            .clone()
            .unwrap_or_else(|| st3::fleet::transport::default_fabric_protocol(&file.fleet_id));
        let _ = std::process::Command::new(fabric)
            .args(["unexpose", &protocol])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    st3::fleet::join::write_private(
        &config.state_dir.join("left-fleet.json"),
        serde_json::to_string(&json!({
            "fleet_id": file.fleet_id,
            "offline": offline,
            "confirmed_by": confirmed,
        }))?
        .as_bytes(),
    )?;
    fs::remove_dir_all(config.state_dir.join("fleet"))?;
    // Local writes resume; the store stays bound to the fleet ID.
    let _: Result<Value> = client
        .post("/v1/internal/fleet/leave/cancel", &json!({}))
        .await;
    Ok(confirmed)
}

async fn run_fleet_remove(client: &Client, config: &Config, args: FleetRemoveArgs) -> Result<()> {
    let person = fleet_person(args.actor, config)?;
    let removal: st3::store::FleetRemoval = client
        .post(
            "/v1/internal/fleet/remove",
            &st3::api::FleetRemoveRequest {
                name: args.name.clone(),
                reason: args.reason,
                person,
            },
        )
        .await?;
    println!(
        "Removed {} (high water {}). Each member refuses it once this removal reaches it.",
        removal.name, removal.high_water
    );
    for invite in &removal.revoked_invites {
        println!("revoked\t{invite}");
    }
    println!(
        "Writes {} made after this node's last exchange with it are not accepted. On {}, if it still runs: st uninstall",
        removal.name, removal.name
    );
    if st3::config::FleetFile::load(&config.state_dir)?.is_some_and(|file| file.legacy_peers) {
        println!(
            "This member still accepts legacy exchanges from config peers, so a machine that keeps the fleet secret can pose as one of them until st fleet migrate --finish."
        );
    }
    Ok(())
}

async fn run_uninstall(endpoint: &Endpoint, args: UninstallArgs) -> Result<()> {
    let config = Config::load_unvalidated(None)?;
    let client = Client::new(endpoint.clone());
    let config_dir = Config::default_path()
        .parent()
        .map(Path::to_path_buf)
        .context("the config path has no directory")?;
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("HOME is not set")?;
    let state_home = config
        .state_dir
        .parent()
        .map(Path::to_path_buf)
        .context("the state directory has no parent")?;
    let data_dir = data_home.join("st3");
    let manifest: Option<Value> = fs::read(data_dir.join("install.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let binaries = manifest
        .as_ref()
        .and_then(|manifest| manifest["files"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|path| path.as_str().map(PathBuf::from))
        .collect::<Vec<_>>();
    let st2_state = state_home.join("st2");
    let st2_only_hooks = fs::read_dir(&st2_state)
        .is_ok_and(|entries| entries.flatten().all(|entry| entry.file_name() == "hooks"));
    let mut paths = vec![
        config.state_dir.clone(),
        config_dir,
        data_dir,
        config.socket.clone(),
        config.client_gateway_socket.clone(),
    ];
    if st2_only_hooks {
        paths.push(st2_state);
    }
    if !args.keep_binaries {
        paths.extend(binaries.iter().cloned());
    }
    paths.sort();
    paths.dedup();
    if args.dry_run {
        for path in &paths {
            println!("remove\t{}", path.display());
        }
        println!("remove\tthe st3 user services, if installed");
        if manifest.is_none() && !args.keep_binaries {
            println!(
                "keep\tthe st3 executables: no release install manifest; remove them the way you installed them"
            );
        }
        return Ok(());
    }
    anyhow::ensure!(
        args.yes,
        "st uninstall erases this machine's st3 state, settings, and services; run it again with --yes, or --dry-run to list them"
    );
    let daemon_running = client.get::<Value>("/v1/health").await.is_ok();
    if let Some(file) = st3::config::FleetFile::load(&config.state_dir)?
        && file.removed.is_none()
    {
        if args.offline {
            fleet_leave(&client, &config, "person/uninstall", true, true, 0).await?;
        } else {
            anyhow::ensure!(
                daemon_running,
                "start st3 so this machine can leave its fleet first, or pass --offline"
            );
            let person = fleet_person(args.actor.clone(), &config)?;
            fleet_leave(&client, &config, &person, false, false, 600).await?;
        }
    }
    let local_only = config.fleet_id.is_none()
        && !config.state_dir.join("left-fleet.json").exists()
        && !config.state_dir.join("fleet").exists()
        && config.state_dir.join("claims.sqlite3").exists();
    anyhow::ensure!(
        !local_only || args.erase_local_graph,
        "this machine's graph exists nowhere else; pass --erase-local-graph to erase it"
    );
    if !args.no_service && services_installed() {
        st3::service::stop_owned_runtimes(&config)?;
        st3::service::uninstall()?;
    } else if client.get::<Value>("/v1/health").await.is_ok() {
        anyhow::bail!("stop st3 up and st3 replication-worker, then run st uninstall --yes again");
    }
    for path in &paths {
        let result = if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        if let Err(error) = result
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("could not remove {}: {error}", path.display());
        }
    }
    let remaining = paths
        .iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    for path in &remaining {
        println!("remains\t{}", path.display());
    }
    println!(
        "Nothing else to remove needs this user. If you installed the Claude Code policy, remove it as root: st3 claude-channel uninstall-policy"
    );
    anyhow::ensure!(remaining.is_empty(), "some st3 files remain");
    println!("uninstalled");
    Ok(())
}

#[derive(Args)]
struct FleetModeArgs {
    /// listening or dial-out.
    mode: String,
    /// The replication port when switching to listening.
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    no_service: bool,
}

#[derive(Args)]
struct FleetMigrateArgs {
    /// A migration code from `st fleet invite NAME --migrate`, or - to read it from standard input.
    code: Option<String>,
    #[arg(long)]
    code_file: Option<PathBuf>,
    #[arg(long)]
    fabric_inbox: bool,
    /// Make this machine the anchor: the first machine of the fleet to migrate.
    #[arg(long, conflicts_with_all = ["code", "code_file", "fabric_inbox", "finish", "unfinish"])]
    anchor: bool,
    /// Stop accepting legacy exchanges once every config peer is a member or removed.
    #[arg(long, conflicts_with = "unfinish")]
    finish: bool,
    /// Accept legacy exchanges again, to roll a machine back to an older build.
    #[arg(long)]
    unfinish: bool,
    /// The Fabric exposure name this machine already uses.
    #[arg(long)]
    fabric_protocol: Option<String>,
    #[arg(long)]
    no_service: bool,
    #[command(flatten)]
    member: FleetMemberArgs,
}

/// Settings for a migrating node: its existing replication port unless one is given.
fn migration_settings(args: &FleetMemberArgs, config: &Config) -> st3::fleet::join::MemberSettings {
    let mut settings = args.settings();
    if settings.port.is_none() {
        settings.port = config
            .peer_listen
            .as_deref()
            .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
            .map(|address| address.port());
    }
    settings
}

async fn run_fleet_migrate(client: &Client, config: &Config, args: FleetMigrateArgs) -> Result<()> {
    if args.finish || args.unfinish {
        let mut file = st3::config::FleetFile::load(&config.state_dir)?
            .context("this machine has not migrated yet")?;
        if args.finish {
            let status: st3::api::FleetStatus = client.get("/v1/internal/fleet/status").await?;
            let waiting = config
                .peers
                .iter()
                .filter(|peer| {
                    let known = status
                        .view
                        .members
                        .iter()
                        .any(|member| member.name == peer.name)
                        || status.view.legacy_removed.contains(&peer.name);
                    !known
                })
                .map(|peer| peer.name.clone())
                .collect::<Vec<_>>();
            anyhow::ensure!(
                waiting.is_empty(),
                "these config peers are neither members nor removed yet: {}",
                waiting.join(", ")
            );
        }
        file.legacy_peers = args.unfinish;
        file.save(&config.state_dir)?;
        if args.finish {
            println!(
                "This machine no longer accepts legacy exchanges. Delete these lines from {}:",
                Config::default_path().display()
            );
            println!("  fleet_id, shared_secret_file, peer_listen, and every [[peers]] entry");
        } else {
            println!("This machine accepts legacy exchanges from config peers again.");
        }
        if !args.no_service && services_installed() {
            st3::service::install(Config::load_with_fleet(None)?)?;
        } else {
            println!("Restart st3 replication-worker for this to take effect.");
        }
        return Ok(());
    }
    let fleet_id = config
        .fleet_id
        .clone()
        .context("this machine has no config-peer fleet to migrate; use st fleet join")?;
    // fleet.toml resolves a relative path under STATE/fleet, and --finish removes the
    // config.toml override, so record the secret file's absolute path now.
    let configured_secret = config
        .shared_secret_file
        .clone()
        .context("config.toml names no shared_secret_file")?;
    let secret_file = fs::canonicalize(&configured_secret).with_context(|| {
        format!(
            "the shared secret file {} is not readable from here; name it with an absolute path in config.toml",
            configured_secret.display()
        )
    })?;
    let use_services = !args.no_service && services_installed();
    if client.get::<Value>("/v1/health").await.is_ok() {
        anyhow::ensure!(
            use_services,
            "stop the running st3 daemon first: nothing may write while this machine migrates"
        );
        st3::service::stop()?;
    }
    let settings = migration_settings(&args.member, config);
    if args.anchor {
        let founded = st3::fleet::join::migrate_anchor(
            &config.state_dir,
            &config.node,
            &fleet_id,
            &secret_file,
            &settings,
            args.fabric_protocol.clone(),
        )?;
        println!(
            "{} is the anchor of fleet {}. It admits itself and signs its history when st3 starts.",
            founded.node, founded.fleet_id
        );
    } else {
        let (code, code_path) = if args.fabric_inbox {
            let path = fabric_inbox_code()?;
            (fs::read_to_string(&path)?, Some(path))
        } else if let Some(path) = &args.code_file {
            (fs::read_to_string(path)?, Some(path.clone()))
        } else {
            match args.code.as_deref() {
                Some("-") => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
                    (text, None)
                }
                Some(code) => (code.to_owned(), None),
                None => (read_code_without_echo()?, None),
            }
        };
        let joined = st3::fleet::join::join(&st3::fleet::join::JoinOptions {
            state_dir: config.state_dir.clone(),
            configured_node: config.node.clone(),
            code: code.trim().to_owned(),
            name: Some(config.node.clone()),
            via: None,
            settings,
            legacy_secret_file: Some(secret_file),
            fabric_protocol: args.fabric_protocol.clone(),
        })
        .await?;
        anyhow::ensure!(
            joined.migrate,
            "that code is a join code; use st fleet join"
        );
        if let Some(path) = code_path {
            let _ = fs::remove_file(path);
        }
        println!(
            "{} migrated to membership in fleet {} through {}.",
            joined.name, joined.fleet_id, joined.sponsor
        );
    }
    if use_services {
        st3::service::install(Config::load_with_fleet(None)?)?;
        println!(
            "The st3 services now run as a fleet member, with legacy exchanges still accepted."
        );
    } else {
        println!(
            "Start st3 up and st3 replication-worker; legacy exchanges stay accepted until st fleet migrate --finish."
        );
    }
    Ok(())
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Args)]
struct UpArgs {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    node: Option<String>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Use an existing PTY registry during an st2-to-st cutover.
    #[arg(long)]
    pty_root: Option<PathBuf>,
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Separate paired-only client gateway socket suitable for a tailnet HTTPS proxy.
    #[arg(long)]
    client_gateway_socket: Option<PathBuf>,
    #[arg(long)]
    peer_listen: Option<String>,
    #[arg(long)]
    fleet_id: Option<String>,
    #[arg(long)]
    shared_secret_file: Option<PathBuf>,
    #[arg(long, value_parser = parse_peer)]
    peer: Vec<PeerConfig>,
    /// Use this pty executable instead of resolving it from the login environment.
    #[arg(long, hide = true)]
    pty_binary: Option<PathBuf>,
}

#[derive(Subcommand)]
enum LaunchCommand {
    /// List current launch conversations; use --all for finished history.
    Ls {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Turn a natural-language request into a durable planning conversation.
    Start(PlanningStartArgs),
    /// Review one launch's request, decisions, candidate, and current status.
    Show(PlanningSessionArgs),
    /// Validate and render the exact candidate that could be approved.
    Preview(PlanningPreviewArgs),
    /// Record an agent-authored candidate revision for a launch.
    Submit(PlanningSubmitArgs),
    /// Add requester feedback and ask the planner for a new candidate.
    Revise(PlanningReviseArgs),
    /// Approve the exact previewed candidate without starting it.
    Approve(PlanningApproveArgs),
    /// Atomically request approval, then idempotently start the published mission revision.
    ApproveAndLaunch(LaunchApproveAndStartArgs),
    /// Start the exact published revision of an already approved launch.
    Run(LaunchRunArgs),
    /// Ask the requester one typed, revisioned question.
    Question(LaunchQuestionArgs),
    /// Record the immutable answer to a launch question.
    Answer(LaunchAnswerArgs),
    /// Stop a launch conversation without publishing its candidate.
    Cancel(PlanningCancelArgs),
    /// Compare two candidate variants field by field.
    Compare(PlanningCompareArgs),
    /// Select the candidate variant the requester should review.
    Propose(PlanningProposeArgs),
}

#[derive(Subcommand)]
enum MissionViewCommand {
    /// Show active runs, standing queues, unstarted missions, and agents by host.
    Tree,
    /// List current missions; use --all for historical terminal missions.
    Ls {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Explain one mission run, its goals, state, work, and usage.
    Show(MissionShowArgs),
    /// Publish exact authored mission KDL after preview and authority checks.
    Publish(MissionPublishArgs),
    /// Start one run from the current ready mission revision.
    Start(MissionRunStartArgs),
    /// Cancel one exact running mission and stop its owned work and runtimes.
    Cancel(MissionCancelArgs),
    /// Show one seat's current claim and its queued mission runs in order; same as `st agents queue AGENT`.
    Queued {
        /// Exact seat subject or its identity without the `agent/` prefix.
        agent: String,
    },
    /// List the open mission requests that one subscription recorded.
    Requests {
        subscription: String,
        /// Include started, cancelled, and failed requests.
        #[arg(long)]
        all: bool,
    },
    /// Start one mission request that an observation held for a person.
    Release(SubscriptionRequestArgs),
    /// Close one pending or held mission request without starting it.
    CancelRequest(SubscriptionRequestArgs),
}

#[derive(Args)]
struct MissionShowArgs {
    mission_or_run: String,
    #[arg(long)]
    follow: bool,
}

#[derive(Args)]
struct MissionPublishArgs {
    /// KDL file to publish; use `-` to read standard input.
    file: PathBuf,
    /// Preview against this exact store index.
    #[arg(long, visible_alias = "at")]
    at_index: Option<u64>,
    /// Complete person or agent subject authoring the publication.
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
}

#[derive(Args)]
struct MissionRunStartArgs {
    mission: String,
    /// Start exactly this published revision, as printed by `missions publish`. A revision
    /// published on another host is awaited briefly while it replicates here.
    #[arg(long)]
    revision: Option<String>,
    /// The full run ID, used as given: `--id release/demo/1` starts `mission-run/release/demo/1`,
    /// and `--id 1` starts `mission-run/1`. Defaults to MISSION/UUIDv7.
    #[arg(long)]
    id: Option<String>,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
    /// Start no work until this mission run completes; fail if it fails or is cancelled.
    #[arg(long, value_name = "RUN")]
    after: Option<String>,
    #[arg(long)]
    follow: bool,
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// Print the exact mission-run KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct MissionCancelArgs {
    /// Exact mission-run subject to cancel.
    mission_run: String,
    /// Why the run is no longer wanted.
    #[arg(long)]
    reason: String,
    /// Concrete human authority carried over the trusted local Unix boundary.
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
}

#[derive(Args)]
struct PlanningStartArgs {
    #[arg(long, required_unless_present = "run", conflicts_with = "run")]
    id: Option<String>,
    #[arg(long)]
    run: Option<String>,
    request: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long = "as", value_parser = parse_person_subject)]
    requester: String,
    #[arg(long, value_parser = ["codex", "claude", "pi", "omp", "opencode"])]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<String>,
    /// Print the planning-session KDL without storing the request or publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct PlanningSessionArgs {
    session: String,
}

#[derive(Args)]
struct PlanningPreviewArgs {
    session: String,
    #[arg(long)]
    variant: Option<String>,
}

#[derive(Args)]
struct PlanningSubmitArgs {
    session: String,
    #[arg(long, default_value = "default")]
    variant: String,
    #[arg(long)]
    markdown: PathBuf,
    #[arg(long)]
    kdl: PathBuf,
    #[arg(long = "as")]
    actor: String,
}

#[derive(Args)]
struct PlanningCompareArgs {
    session: String,
    left: String,
    right: String,
}

#[derive(Args)]
struct PlanningProposeArgs {
    session: String,
    variant: String,
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long)]
    reason: String,
}

#[derive(Args)]
struct PlanningReviseArgs {
    session: String,
    feedback: PathBuf,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
    /// Print the feedback KDL without storing the feedback or publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct PlanningApproveArgs {
    session: String,
    preview_hash: String,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
}

#[derive(Args)]
struct LaunchApproveAndStartArgs {
    session: String,
    preview_hash: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
}

#[derive(Args)]
struct LaunchRunArgs {
    session: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
}

#[derive(Args)]
struct LaunchQuestionArgs {
    session: String,
    question: String,
    #[arg(long = "type", value_parser = parse_launch_decision_type)]
    decision_type: LaunchDecisionType,
    /// Structured JSON option: {"id":"stable-id","label":"Label","description":"optional"}.
    #[arg(long = "option", value_parser = parse_launch_decision_option)]
    options: Vec<LaunchDecisionOption>,
    #[arg(long = "as")]
    actor: String,
}

#[derive(Args)]
struct LaunchAnswerArgs {
    session: String,
    decision: String,
    /// Structured JSON response such as {"type":"multiple-choice","value":["a","b"]}.
    #[arg(value_parser = parse_launch_decision_response)]
    response: LaunchDecisionResponse,
    #[arg(long)]
    explanation: Option<String>,
    #[arg(long, default_value_t = 1)]
    expected_revision: u32,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
}

#[derive(Args)]
struct PlanningCancelArgs {
    session: String,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
    #[arg(long)]
    reason: Option<String>,
    /// Print the cancellation KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Subcommand)]
enum PtyCommand {
    /// List current terminal sessions; use --all for stopped history.
    Ls {
        #[arg(long)]
        all: bool,
        /// Resume the next bounded page returned by an earlier list.
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Attach this terminal interactively to one running terminal member.
    Attach(PtyAttachArgs),
    /// Read one terminal's current screen without taking control.
    Peek(PtySubjectArgs),
    /// Read a terminal screen through the client gateway, including a remote fleet host.
    Screen(PtyScreenArgs),
    /// Create a short-lived client attachment and show its stream details.
    AttachInfo(PtyScreenArgs),
    /// Follow a terminal's screens with an attachment capability until the stream ends.
    Stream(PtyStreamArgs),
    /// Send input through the client gateway to a local or remote terminal.
    InputClient(PtyClientInputArgs),
    /// End a client attachment by its exact attachment ID.
    DetachClient(PtyClientDetachArgs),
    /// Send explicit text or a named key to one running terminal.
    Send(PtySendArgs),
    /// Deliver one supported Unix signal to a terminal member.
    Signal(PtySignalArgs),
}

#[derive(Args)]
struct PtySubjectArgs {
    subject: String,
}

#[derive(Args)]
struct PtyScreenArgs {
    subject: String,
    /// Use this concrete person instead of the person configured for trusted local commands.
    #[arg(long = "as", value_parser = parse_person_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtyStreamArgs {
    subject: String,
    #[arg(long = "as", value_parser = parse_person_subject)]
    person: Option<String>,
    /// The stream capability from `terminals attach-info`; can be set through the environment.
    #[arg(long, env = "ST3_TERMINAL_CAPABILITY")]
    capability: String,
    #[arg(long)]
    incarnation: Option<String>,
    /// Stop after this many screens instead of following until the stream ends.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    count: Option<u64>,
}

#[derive(Args)]
struct PtyClientInputArgs {
    subject: String,
    value: String,
    #[arg(long = "as", value_parser = parse_person_subject)]
    person: Option<String>,
    #[arg(long, conflicts_with = "key")]
    raw: bool,
    #[arg(long, conflicts_with = "raw")]
    key: bool,
}

#[derive(Args)]
struct PtyClientDetachArgs {
    attachment: String,
    /// Runtime incarnation returned by `terminals attach-info`.
    #[arg(long)]
    incarnation: String,
    #[arg(long = "as", value_parser = parse_person_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtyAttachArgs {
    subject: String,
    /// Allow an attachment from inside another PTY session.
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct PtySendArgs {
    subject: String,
    value: String,
    #[arg(long, conflicts_with = "key")]
    raw: bool,
    #[arg(long, conflicts_with = "raw")]
    key: bool,
}

#[derive(Args)]
struct PtySignalArgs {
    subject: String,
    #[arg(value_parser = ["interrupt", "hangup", "user-1", "user-2"])]
    signal: String,
}

#[derive(Args)]
struct InspectArgs {
    subject: String,
}

#[derive(Args)]
struct TraceArgs {
    subject: Option<String>,
    #[arg(long)]
    owner_run: Option<String>,
    #[arg(long, default_value_t = 100)]
    limit: usize,
    #[arg(long)]
    after_index: Option<u64>,
    #[arg(short = 'f', long)]
    follow: bool,
}

#[derive(Args)]
struct WaitArgs {
    subject: String,
    /// Interrupt the wait for this exact agent's messages or newly ready work.
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long = "for", default_value = "ready")]
    condition: String,
    #[arg(long, default_value = "10m")]
    timeout: String,
}

#[derive(Args)]
struct NowArgs {
    /// Use this concrete person instead of the person configured for trusted local commands.
    #[arg(long = "as", value_parser = parse_person_subject)]
    person: Option<String>,
    #[arg(long)]
    owner_run: Option<String>,
    /// Include explicitly historical rows in addition to the actionable default.
    #[arg(long)]
    all: bool,
    #[arg(long)]
    cursor: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

#[derive(Args)]
struct MachinesArgs {
    /// Include historical and discovered hosts beyond the current configured fleet.
    #[arg(long)]
    all: bool,
    #[arg(long)]
    cursor: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

#[derive(Args)]
struct ActivityArgs {
    #[arg(long)]
    after: Option<String>,
    #[arg(long, default_value_t = 100)]
    limit: usize,
    #[arg(short = 'f', long)]
    follow: bool,
    /// Include lease renewals and other non-material heartbeat records.
    #[arg(long)]
    all: bool,
}

#[derive(Subcommand)]
enum DevicesCommand {
    /// List paired devices visible to the authenticated person.
    Ls,
    /// Begin local pairing for one named person and device.
    Pair {
        device_name: String,
        /// Delegate every current client scope to this trusted device.
        #[arg(long)]
        full_control: bool,
    },
    /// Revoke one paired device.
    Revoke {
        device: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Args)]
struct DevicesArgs {
    /// Concrete human authority carried over the trusted local Unix boundary.
    #[arg(long = "as", value_parser = parse_person_subject, global = true)]
    person: Option<String>,
    /// Include expired and revoked device history.
    #[arg(long)]
    all: bool,
    #[arg(long)]
    cursor: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(subcommand)]
    command: Option<DevicesCommand>,
}

#[derive(Subcommand)]
enum SubjectCommand {
    /// Show one typed subject card.
    Show(InspectArgs),
    /// Show bounded immutable history for one subject.
    History(TraceArgs),
}

#[derive(Subcommand)]
enum TraceCommand {
    /// Show or follow bounded graph history.
    Show(TraceArgs),
    /// Wait until a graph condition is true using bounded retry/long-poll requests.
    Wait(WaitArgs),
}

#[derive(Args)]
struct DoctorArgs {
    #[arg(long)]
    strict: bool,
}

#[derive(Subcommand)]
enum RepairCommand {
    /// Compute the exact read-only repair plan and approval token.
    DryRun,
    /// Apply exactly the plan identified by a dry-run token.
    Apply { token: String },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Install and start the st user services for this machine.
    Install {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Show whether the daemon and replication services are installed and running.
    Status,
    /// Explain the one-time macOS permissions for service-owned work.
    Permissions {
        /// Open the matching System Settings pages on macOS.
        #[arg(long)]
        open: bool,
    },
    /// Restart st after configuration or binary changes.
    Restart {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Irreversibly erase local st state and restart an empty daemon.
    Reset {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Stop and remove st user services while preserving state files.
    Uninstall,
}

#[derive(Subcommand)]
enum ClaudeChannelCommand {
    /// Install or update the user plugin and its machine approval policy.
    Install {
        /// Install only the user plugin. An administrator will manage the machine policy.
        #[arg(long)]
        no_policy: bool,
    },
    /// Verify the embedded files, Claude registration, plugin, and machine policy.
    Status,
    /// Remove the user plugin, marketplace, embedded files, and machine policy.
    Uninstall {
        /// Keep the machine approval policy in place.
        #[arg(long)]
        keep_policy: bool,
    },
    /// Write only the machine policy. The main installer runs this through sudo.
    #[command(hide = true)]
    InstallPolicy,
    /// Remove only the ST-owned machine policy fragment.
    #[command(hide = true)]
    UninstallPolicy,
}

#[derive(Subcommand)]
enum ReplicationCommand {
    /// Show fleet receipt, validation, projection, and peer health.
    Status,
    /// List invalid and unknown replicated records.
    Invalid {
        #[arg(long)]
        all: bool,
    },
    /// Show one replicated record diagnostic.
    Inspect { record: String },
    /// Compare local logical digests with one peer's last signed response.
    Diff { peer: String },
    /// Replace one invalid record with an admitted claim.
    Repair {
        record: String,
        #[arg(long = "with")]
        replacement_claim: String,
        #[arg(long)]
        reason: String,
        #[arg(long = "as")]
        actor: String,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum DocCommand {
    /// Store a regular file as one immutable named document version.
    Put {
        file: PathBuf,
        #[arg(long = "as")]
        name: String,
    },
    /// Read exact document bytes by immutable name-and-hash reference.
    Get {
        reference: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// List selected document bindings; use --all for immutable version history.
    Ls {
        name: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue from a previous page's next_cursor.
        #[arg(long)]
        cursor: Option<String>,
    },
}

#[derive(Subcommand)]
enum ImportCommand {
    /// List running native sessions; use --all for resumable saved history.
    Ls {
        #[arg(long)]
        all: bool,
        /// Resume the next bounded page returned by an earlier list.
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show the exact native identity, workspace, process fence, and importability.
    Show { session: String },
    /// Stop an exactly identified running harness and resume it in a durable st mission.
    Run {
        session: String,
        #[arg(long = "as", value_parser = parse_person_subject)]
        person: String,
    },
}

#[derive(Args)]
struct AgentsArgs {
    #[arg(long)]
    status: Option<String>,
    #[arg(long)]
    enrich: bool,
    /// Include stopped, superseded, terminal-owner, and historical eval agents.
    #[arg(long)]
    all: bool,
    /// Resume the next bounded page returned by an earlier list.
    #[arg(long)]
    cursor: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

#[derive(Subcommand)]
enum AgentsCommand {
    /// List operational agents; use --all for stopped and historical agents.
    Ls(AgentsArgs),
    /// Group operational agents beneath the mission runs that own them.
    Tree(AgentsArgs),
    /// Show one exact agent, including its owner and operational annotation.
    Show {
        subject: String,
        /// Include a historical agent that is absent from the operational default.
        #[arg(long)]
        all: bool,
    },
    /// Preview and apply one KDL file containing durable agent seats.
    Apply(AgentApplyArgs),
    /// Start or update one durable typed-harness seat.
    Start(AgentStartArgs),
    /// Stop one exact durable seat.
    Stop(AgentStopArgs),
    /// Show one seat's current claim and its queued mission runs in order, or move a run.
    /// The show form is also available as `st missions queued AGENT`.
    Queue(AgentQueueArgs),
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct AgentQueueArgs {
    #[command(subcommand)]
    command: Option<AgentQueueCommand>,
    /// Exact seat subject or its identity without the `agent/` prefix.
    #[arg(required = true)]
    agent: Option<String>,
}

#[derive(Subcommand)]
enum AgentQueueCommand {
    /// Move one queued mission run. A step the seat already holds stays held.
    Move(AgentQueueMoveArgs),
}

#[derive(Args)]
#[command(group(
    clap::ArgGroup::new("placement")
        .required(true)
        .args(["top", "bottom", "before", "after"])
))]
struct AgentQueueMoveArgs {
    /// Exact seat subject or its identity without the `agent/` prefix.
    agent: String,
    /// Queued mission run to move.
    run: String,
    /// Put the run first in the seat's queue.
    #[arg(long)]
    top: bool,
    /// Put the run last in the seat's queue.
    #[arg(long)]
    bottom: bool,
    /// Put the run directly before another queued run.
    #[arg(long, value_name = "RUN")]
    before: Option<String>,
    /// Put the run directly after another queued run.
    #[arg(long, value_name = "RUN")]
    after: Option<String>,
    /// Why the order changed; recorded with the move.
    #[arg(long)]
    reason: Option<String>,
    /// Person or agent making the move; defaults to `person` in the st config. An agent needs
    /// `queue-authority { move "SEAT" }` for this seat in its declaration.
    #[arg(long = "as", value_parser = parse_queue_move_actor)]
    actor: Option<String>,
}

#[derive(Args)]
struct AgentApplyArgs {
    /// KDL file to publish; use `-` to read standard input.
    file: PathBuf,
    /// Complete person or agent subject authoring the publication.
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
}

#[derive(Args)]
struct AgentStartArgs {
    /// Stable seat identity. Slash-qualified identities are preserved exactly after `agent/`.
    identity: String,
    #[arg(long, default_value = "claude", value_parser = ["claude", "codex", "pi", "omp", "opencode"])]
    harness: String,
    #[arg(long)]
    host: Option<String>,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<String>,
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long = "arg")]
    arguments: Vec<String>,
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// Print the exact seat KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct AgentStopArgs {
    /// Exact seat subject or its identity without the `agent/` prefix.
    subject: String,
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// Print the exact stop KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct ClaimArgs {
    subject: String,
    kind: String,
    #[arg(long)]
    actor: Option<String>,
    #[arg(long = "field", value_parser = parse_field)]
    fields: Vec<(String, Value)>,
    #[arg(long)]
    evidence: Vec<String>,
    /// Return the same logical result when this graph-wide key is retried.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct HarnessDiagnosticArgs {
    #[arg(long = "as")]
    actor: String,
    #[arg(long)]
    code: String,
    #[arg(long)]
    reason: String,
    #[arg(long, default_value = "error", value_parser = ["warning", "error"])]
    severity: String,
    #[arg(long, default_value = "active")]
    status: String,
    #[arg(long, env = "ST3_INCARNATION")]
    incarnation: Option<String>,
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// List registered subject families.
    Subjects,
    /// List registered resource kinds.
    Resources,
    /// List claim kinds, optionally for one subject.
    Claims {
        #[arg(long)]
        subject: Option<String>,
    },
    /// Show one claim kind.
    Show { kind: String },
    /// Export the complete registry.
    Export,
}

#[derive(Args)]
struct SubscriptionRequestArgs {
    request: String,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
    #[arg(long)]
    reason: String,
}

#[derive(Subcommand)]
enum AttentionCommand {
    /// List all current human attention items.
    Ls {
        #[arg(long = "as", value_parser = parse_person_subject)]
        actor: Option<String>,
        /// Include resolved and historical attention.
        #[arg(long)]
        all: bool,
        /// Resume the next bounded page returned by an earlier list.
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Explain one attention item and show the exact available actions.
    Show {
        subject: String,
        #[arg(long = "as", value_parser = parse_person_subject)]
        actor: Option<String>,
    },
    /// Request attention after an explicit fault.
    ///
    /// The item stays in `st now` until its reviewer resolves it or you withdraw it with
    /// `st attention withdraw` once the condition clears. It also leaves `now` on its own:
    ///
    /// - at once, when a `step-run/` or `run-generation/` target is no longer current;
    /// - otherwise, once every other target has ended after the request: a `mission/` retired or
    ///   cancelled, a `mission-run/` terminal, an `attention/` item resolved or its gate no
    ///   longer pending, or an `agent/` stopped or ready on a later incarnation.
    ///
    /// `resource/` and `doc/` targets are context and never end an item. A target of any other
    /// kind, or one that had already ended when you made the request, keeps it open.
    #[command(verbatim_doc_comment)]
    Request(AttentionRequestArgs),
    /// Resolve or dismiss an explicit attention request.
    Resolve(AttentionResolveArgs),
    /// Withdraw an obsolete attention request as its original requester.
    Withdraw(AttentionWithdrawArgs),
    /// Approve one person-owned gate or launch review.
    Approve(ReviewArgs),
    /// Reject one person-owned gate or launch review.
    Reject(ReviewArgs),
    /// Ask a feedback-mode step to change its work and rerun.
    RequestChanges(FeedbackReviewArgs),
}

#[derive(Args)]
struct AttentionRequestArgs {
    #[arg(long = "for", value_parser = parse_person_subject)]
    reviewer: String,
    #[arg(long)]
    title: String,
    #[arg(long)]
    reason: String,
    #[arg(long, value_parser = ["warning", "error"], default_value = "error")]
    severity: String,
    /// A subject this fault is about; repeat for several. Its kind decides whether it can end the
    /// item on its own.
    #[arg(long = "target")]
    targets: Vec<String>,
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    /// Resolve the item on its own once every target meets this `st trace wait` condition,
    /// such as `completed` or `stopped`; it needs at least one --target.
    #[arg(long, value_name = "CONDITION")]
    until: Option<String>,
}

#[derive(Args)]
struct AttentionResolveArgs {
    subject: String,
    #[arg(long, value_parser = ["resolved", "dismissed"])]
    outcome: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
}

#[derive(Args)]
struct AttentionWithdrawArgs {
    subject: String,
    #[arg(long)]
    reason: String,
    #[arg(long = "as")]
    actor: String,
}

#[derive(Subcommand)]
enum WorkCommand {
    /// List current actionable work; use --as to filter one agent or --all for history.
    Ls {
        #[arg(long = "as")]
        actor: Option<String>,
        #[arg(long)]
        all: bool,
        /// Resume the next bounded page returned by an earlier list.
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Explain one work item, its owner, readiness, lease, and evidence.
    Show { subject: String },
    /// Acquire one ready work item with the current harness incarnation.
    Claim(WorkActionArgs),
    /// Extend the live lease for work this incarnation still owns.
    Renew(WorkActionArgs),
    /// Record a material progress update without changing ownership.
    Progress(WorkActionArgs),
    /// Finish claimed work and attach its durable evidence.
    Complete(WorkActionArgs),
    /// Fail claimed work with an actionable reason and evidence.
    Fail(WorkActionArgs),
    /// Give claimed work back so another eligible agent can take it.
    Release(WorkActionArgs),
    /// Wake one ready assignee through its supported harness driver.
    Wake(WorkWakeArgs),
    /// Retry one failed step; this reopens its failed run when that step was the only failure.
    Retry(WorkRetryArgs),
    /// Publish the exact ready mission produced by one claimed step.
    PublishMission(WorkPublishMissionArgs),
    /// Propose a fenced revision to the mission that owns this work.
    Revise(WorkReviseArgs),
    /// Inspect or decide one mission revision proposal and its generations.
    Revision {
        #[command(subcommand)]
        command: WorkRevisionCommand,
    },
}

#[derive(Args)]
struct WorkWakeArgs {
    subject: String,
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long, default_value = "manual wake requested")]
    reason: String,
}

#[derive(Args)]
struct WorkRetryArgs {
    subject: String,
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long)]
    reason: String,
}

#[derive(Subcommand)]
enum WorkRevisionCommand {
    /// Show the current revision proposal for one mission run.
    Show { run: String },
    /// List every immutable generation created for one mission run.
    Generations { run: String },
    /// Explain one exact generation and its work state.
    Generation { generation: String },
    /// Approve an exact proposal preview and permit its cutover.
    Approve {
        proposal: String,
        preview_hash: String,
        #[arg(long = "as")]
        actor: Option<String>,
    },
    /// Cancel a pending revision proposal without changing the live generation.
    Cancel {
        proposal: String,
        #[arg(long = "as")]
        actor: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Args)]
struct WorkActionArgs {
    subject: String,
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long, env = "ST3_INCARNATION")]
    incarnation: Option<String>,
    #[arg(long)]
    summary: Option<String>,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long)]
    evidence: Vec<String>,
}

#[derive(Args)]
struct WorkReviseArgs {
    run: String,
    file: PathBuf,
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long)]
    reason: String,
    /// Print the revision KDL without publishing the candidate mission or revision.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct WorkPublishMissionArgs {
    subject: String,
    file: PathBuf,
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long, env = "ST3_INCARNATION")]
    incarnation: Option<String>,
}

#[derive(Subcommand)]
enum MessageCommand {
    /// Send one durable normalized message to a person or agent.
    Send(MessageSendArgs),
    /// List the current mailbox for one explicit identity.
    Ls(MessageListArgs),
    /// Read exact messages and optionally mark them read or archived.
    Read(MessageReadArgs),
    /// Reply to one canonical message ID while preserving its thread.
    Reply(MessageReplyArgs),
    /// Close exact messages after their related action is complete.
    Archive(MessageArchiveArgs),
    /// Render the bounded conversation thread around one message.
    Thread(MessageReferenceArgs),
    /// List normalized harness sessions available for native conversation views.
    Sessions {
        #[arg(long = "as", value_parser = parse_person_subject)]
        actor: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Render one normalized session timeline, including tools and usage.
    Timeline {
        session: String,
        #[arg(long = "as", value_parser = parse_person_subject)]
        actor: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue toward older entries using the preceding response's next cursor.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Follow the visible normalized conversation; JSON output is one entry per line.
    Follow {
        session: String,
        #[arg(long = "as", value_parser = parse_person_subject)]
        actor: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Write a disposable mailbox tree for translated tools.
    Export { directory: PathBuf },
}

#[derive(Args)]
struct MessageSendArgs {
    to: String,
    #[arg(short = 'm', long)]
    body: String,
    #[arg(long)]
    subject: Option<String>,
    #[arg(long)]
    in_reply_to: Option<String>,
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,
    #[arg(long = "from", alias = "as")]
    from: String,
    /// Print the generated message mission KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct MessageListArgs {
    /// Mailbox identity; defaults to the non-empty ST_AGENT value.
    identity: Option<String>,
    /// The same mailbox identity, spelled like `conversations read --as`.
    #[arg(long = "as", conflicts_with = "identity")]
    actor: Option<String>,
    #[arg(long)]
    archive: bool,
    #[arg(long)]
    count: bool,
    #[arg(long = "from")]
    sender: Option<String>,
}

#[derive(Args)]
struct MessageReadArgs {
    #[arg(num_args = 1..)]
    references: Vec<String>,
    #[arg(long)]
    raw: bool,
    #[arg(long)]
    archive: bool,
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct MessageReplyArgs {
    reference: String,
    #[arg(short = 'm', long)]
    body: String,
    #[arg(long)]
    subject: Option<String>,
    #[arg(long = "from", alias = "as")]
    from: String,
    /// Print the generated reply mission KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct MessageArchiveArgs {
    #[arg(num_args = 1..)]
    references: Vec<String>,
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct MessageReferenceArgs {
    reference: String,
    #[arg(long)]
    tree: bool,
}

#[derive(Args)]
struct ReviewArgs {
    target: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
}

#[derive(Args)]
struct FeedbackReviewArgs {
    target: String,
    #[arg(long)]
    reason: String,
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: String,
}

#[derive(Args)]
struct DriverArgs {
    #[arg(value_parser = ["claude", "claude-mcp", "codex", "pi", "pi-channel", "omp", "omp-channel", "opencode", "exec"])]
    driver: String,
    #[arg(long, env = "ST_AGENT")]
    subject: Option<String>,
    #[arg(long)]
    identity: Option<String>,
    #[arg(last = true)]
    argv: Vec<String>,
}

#[derive(Args)]
struct CompletionsArgs {
    #[arg(value_enum)]
    shell: CompletionShell,
}

#[derive(Clone, Copy, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug)]
struct CommandExit(u8);

impl std::fmt::Display for CommandExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the command selected exit status {}", self.0)
    }
}

impl std::error::Error for CommandExit {}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<CommandExit>() {
                return ExitCode::from(exit.0);
            }
            eprintln!("st: {error:#}");
            let message = error.to_string();
            if daemon_is_unreachable(&error) {
                ExitCode::from(5)
            } else if message.contains("stale-subject") {
                ExitCode::from(3)
            } else if message.contains("terminal status selected")
                || message.contains("wait timed out")
            {
                ExitCode::from(4)
            } else {
                ExitCode::from(2)
            }
        }
    }
}

static DAEMON_WAIT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// A command client that waits out a daemon restart for the `--daemon-wait` window.
fn cli_client(endpoint: &Endpoint) -> Client {
    Client::new(endpoint.clone())
        .with_outage_wait(DAEMON_WAIT.get().copied().unwrap_or_default(), true)
}

/// Exit status 5 means the daemon was unreachable, whichever client made the request.
fn daemon_is_unreachable(error: &anyhow::Error) -> bool {
    st3::client::daemon_unreachable(error).is_some()
        || error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<GeneratedClientError>(),
                Some(GeneratedClientError::Unreachable(_))
            )
        })
}

async fn run(cli: Cli) -> Result<()> {
    let own = std::env::var("ST_AGENT").ok();
    let mission_run = std::env::var("ST_MISSION_RUN").ok();
    guard_mutating_cli_actor(&cli.command, own.as_deref(), mission_run.as_deref())?;
    if let Command::Up(args) = cli.command {
        return run_up(args).await;
    }
    if let Command::ReplicationWorker(args) = cli.command {
        let mut config = Config::load_unvalidated(args.config.as_deref())?;
        if let Some(value) = args.node {
            config.node = value;
        }
        if let Some(value) = args.state_dir {
            config.state_dir = value;
        }
        if let Some(value) = args.socket {
            config.socket = value;
        }
        if let Some(value) = args.peer_listen {
            config.peer_listen = Some(value);
        }
        if let Some(value) = args.fleet_id {
            config.fleet_id = Some(value);
        }
        if let Some(value) = args.shared_secret_file {
            config.shared_secret_file = Some(value);
        }
        if !args.peer.is_empty() {
            config.peers = args.peer;
        }
        config.apply_fleet_file()?;
        return st3::peer::run_worker(config).await;
    }
    let config = Config::load_unvalidated(None)?;
    let endpoint = cli
        .endpoint
        .or_else(|| std::env::var("ST3_ENDPOINT").ok())
        .as_deref()
        .map(Endpoint::parse)
        .unwrap_or_else(|| Endpoint::Unix(config.socket.clone()));
    let _ = DAEMON_WAIT.set(Duration::from_secs(cli.daemon_wait));
    let client = cli_client(&endpoint);
    // Drivers outlive daemon restarts and handle an outage in their own loops; doctor reports one.
    let immediate = Client::new(endpoint.clone());
    match cli.command {
        Command::Up(_) => unreachable!(),
        Command::ReplicationWorker(_) => unreachable!(),
        Command::Now(args) => run_now(&endpoint, config.person.as_deref(), args, cli.json).await,
        Command::Launch { command } => {
            run_launch(&client, &endpoint, command, &config.planner, cli.json).await
        }
        Command::Missions { command } => {
            run_mission_view(&client, &endpoint, command, cli.json).await
        }
        Command::Attention { command } => {
            run_attention(
                &client,
                &endpoint,
                config.person.as_deref(),
                command,
                cli.json,
            )
            .await
        }
        Command::Machines(args) => run_machines(&endpoint, args, cli.json).await,
        Command::Agents { command } => {
            run_agents(&endpoint, config.person.as_deref(), command, cli.json).await
        }
        Command::Conversations { command } => {
            run_message(
                &client,
                &endpoint,
                config.person.as_deref(),
                command,
                cli.json,
            )
            .await
        }
        Command::Activity(args) => run_activity(&endpoint, args, cli.json).await,
        Command::Devices(args) => {
            run_devices(endpoint.clone(), config.person.as_deref(), args, cli.json).await
        }
        Command::Work { command } => run_work(&client, &endpoint, command, cli.json).await,
        Command::Terminals { command } => {
            run_pty(
                &client,
                &endpoint,
                config.person.as_deref(),
                command,
                cli.json,
            )
            .await
        }
        Command::Doctor(args) => run_doctor(&immediate, args, cli.json).await,
        Command::Repair { command } => run_repair(&client, command, cli.json).await,
        Command::Replication { command } => run_replication(&client, command, cli.json).await,
        Command::Fleet { command } => run_fleet(&endpoint, command, cli.json).await,
        Command::Uninstall(args) => run_uninstall(&endpoint, args).await,
        Command::Service { command } => run_service(command, cli.json),
        Command::ClaudeChannel { command } => run_claude_channel(command),
        Command::Subject { command } => run_subject(&client, command, cli.json).await,
        Command::Claim(args) => run_claim(&client, args, cli.json).await,
        Command::Diagnostic(args) => run_harness_diagnostic(&client, args, cli.json).await,
        Command::Trace { command } => run_trace_command(&client, command, cli.json).await,
        Command::Schema { command } => run_schema(&client, command, cli.json).await,
        Command::Documents { command } => run_doc(&client, command, cli.json).await,
        Command::Import { command } => run_import(&endpoint, command, cli.json).await,
        Command::Completions(args) => {
            let shell = match args.shell {
                CompletionShell::Bash => clap_complete::Shell::Bash,
                CompletionShell::Zsh => clap_complete::Shell::Zsh,
                CompletionShell::Fish => clap_complete::Shell::Fish,
            };
            clap_complete::generate(shell, &mut Cli::command(), "st", &mut std::io::stdout());
            Ok(())
        }
        Command::Driver(args) => run_driver(&immediate, args, cli.catalog.as_deref()).await,
    }
}

/// Guard every explicit actor on commands that change graph state before any request is sent.
/// A harness may use its own agent identity, but cannot borrow a peer or person identity.
fn guard_mutating_cli_actor(
    command: &Command,
    own: Option<&str>,
    mission_run: Option<&str>,
) -> Result<()> {
    let Some(own) = own.filter(|own| own.starts_with("agent/")) else {
        return Ok(());
    };
    let actor = match command {
        Command::Missions { command } => match command {
            MissionViewCommand::Publish(args) => Some(args.actor.as_str()),
            MissionViewCommand::Start(args) => Some(args.actor.as_str()),
            MissionViewCommand::Cancel(args) => Some(args.actor.as_str()),
            _ => None,
        },
        Command::Agents { command } => match command {
            AgentsCommand::Apply(args) => Some(args.actor.as_str()),
            AgentsCommand::Start(args) => Some(args.actor.as_str()),
            AgentsCommand::Stop(args) => Some(args.actor.as_str()),
            AgentsCommand::Queue(args) => match &args.command {
                Some(AgentQueueCommand::Move(args)) => Some(args.actor.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("a harness queue move needs explicit --as {own}; it cannot use the configured person")
                })?),
                None => None,
            },
            _ => None,
        },
        Command::Work { command } => match command {
            WorkCommand::Claim(args) | WorkCommand::Renew(args) | WorkCommand::Progress(args)
            | WorkCommand::Complete(args) | WorkCommand::Fail(args) | WorkCommand::Release(args) => args.actor.as_deref(),
            WorkCommand::Wake(args) => args.actor.as_deref(),
            WorkCommand::PublishMission(args) => args.actor.as_deref(),
            WorkCommand::Revise(args) => args.actor.as_deref(),
            WorkCommand::Revision { command } => match command {
                WorkRevisionCommand::Approve { actor, .. } | WorkRevisionCommand::Cancel { actor, .. } => actor.as_deref(),
                _ => None,
            },
            _ => None,
        },
        Command::Attention { command } => match command {
            AttentionCommand::Request(args) => args.actor.as_deref(),
            AttentionCommand::Resolve(args) => Some(args.actor.as_str()),
            AttentionCommand::Withdraw(args) => Some(args.actor.as_str()),
            AttentionCommand::Approve(args) | AttentionCommand::Reject(args) => Some(args.actor.as_str()),
            AttentionCommand::RequestChanges(args) => Some(args.actor.as_str()),
            _ => None,
        },
        Command::Launch { command } => match command {
            LaunchCommand::Start(args) => Some(args.requester.as_str()),
            LaunchCommand::Submit(args) => Some(args.actor.as_str()),
            LaunchCommand::Revise(args) => Some(args.actor.as_str()),
            LaunchCommand::Approve(args) => Some(args.actor.as_str()),
            LaunchCommand::ApproveAndLaunch(args) => Some(args.actor.as_str()),
            LaunchCommand::Run(args) => Some(args.actor.as_str()),
            LaunchCommand::Question(args) => Some(args.actor.as_str()),
            LaunchCommand::Answer(args) => Some(args.actor.as_str()),
            LaunchCommand::Cancel(args) => Some(args.actor.as_str()),
            LaunchCommand::Propose(args) => args.actor.as_deref(),
            _ => None,
        },
        Command::Claim(args) => args.actor.as_deref(),
        Command::Diagnostic(args) => Some(args.actor.as_str()),
        _ => None,
    };
    if let Some(actor) = actor {
        if actor.starts_with("person/") || actor == "requester" {
            anyhow::bail!(
                "this harness is `{own}` (ST_AGENT) and cannot act as `{actor}` on a mutating command; request a person through `st attention request --as \"$ST_AGENT\"`"
            );
        }
        if let Some(message) = foreign_agent_actor(actor, Some(own), mission_run) {
            anyhow::bail!(message);
        }
    }
    Ok(())
}

fn run_claude_channel(command: ClaudeChannelCommand) -> Result<()> {
    match command {
        ClaudeChannelCommand::Install { no_policy } => {
            st2::claude_channel::install_st3(no_policy).map(|_| ())
        }
        ClaudeChannelCommand::Status => st2::claude_channel::status_st3(),
        ClaudeChannelCommand::Uninstall { keep_policy } => {
            st2::claude_channel::uninstall_st3(keep_policy)
        }
        ClaudeChannelCommand::InstallPolicy => {
            st2::claude_channel::install_st3_policy().map(|_| ())
        }
        ClaudeChannelCommand::UninstallPolicy => st2::claude_channel::uninstall_st3_policy(),
    }
}

async fn run_up(args: UpArgs) -> Result<()> {
    let mut config = Config::load_unvalidated(args.config.as_deref())?;
    if let Some(node) = args.node {
        config.node = node;
    }
    if let Some(state_dir) = args.state_dir {
        config.state_dir = state_dir;
    }
    if let Some(pty_root) = args.pty_root {
        config.pty_root = Some(pty_root);
    }
    if let Some(socket) = args.socket {
        config.socket = socket;
    }
    if let Some(socket) = args.client_gateway_socket {
        config.client_gateway_socket = socket;
    }
    if let Some(peer_listen) = args.peer_listen {
        config.peer_listen = Some(peer_listen);
    }
    if let Some(fleet_id) = args.fleet_id {
        config.fleet_id = Some(fleet_id);
    }
    if let Some(shared_secret_file) = args.shared_secret_file {
        config.shared_secret_file = Some(shared_secret_file);
    }
    if !args.peer.is_empty() {
        config.peers = args.peer;
    }
    config.apply_fleet_file()?;
    config.validate()?;
    st2::hooks::ensure_installed().context(
        "publishing this st binary's required lifecycle hook set before starting the daemon",
    )?;
    fs::create_dir_all(&config.state_dir)?;
    let store = Arc::new(Store::open(
        &config.state_dir.join("claims.sqlite3"),
        &config.node,
    )?);
    if let Some(fleet_id) = &config.fleet_id {
        store.bind_fleet(fleet_id)?;
    }
    // A member pins its anchor, applies its writer floor, and signs with its key before it
    // writes anything, so every local batch after this point is signed.
    st3::fleet::activate(&store, &config)?;
    let admission = store.validate_replication_backlog()?;
    store.apply_replication_repairs()?;
    let projected = store.project_replication_backlog()?;
    if !projected {
        eprintln!(
            "st: the replicated projection is stale; the daemon will use its last good graph"
        );
    }
    if admission.invalid != 0 || admission.unknown != 0 {
        eprintln!(
            "st: replication has {} invalid and {} unknown records",
            admission.invalid, admission.unknown
        );
    }
    store.append_claim(&ClaimInput {
        subject: format!("daemon/{}", config.node),
        kind: "daemon.started".into(),
        actor: None,
        fields: BTreeMap::from([
            ("status".into(), Value::String("running".into())),
            ("pid".into(), Value::from(std::process::id())),
            (
                "version".into(),
                Value::String(env!("CARGO_PKG_VERSION").into()),
            ),
            (
                "schema".into(),
                Value::String(st3_schema::SCHEMA_NAME.into()),
            ),
            (
                "schema_digest".into(),
                Value::String(st3_schema::registry().digest()),
            ),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!(
            "daemon-start:{}:{}",
            config.node,
            std::process::id()
        )),
    })?;
    let notify = Arc::new(Notify::new());
    let (event_notify, _event_receiver) = watch::channel(0_u64);
    let pty_root = config
        .pty_root
        .clone()
        .unwrap_or_else(|| config.state_dir.join("pty"));
    let login_environment = st3::environment::snapshot()?;
    st_runtime::initialize_isolation(&login_environment);
    let pty_binary = match args.pty_binary.clone() {
        Some(pty_binary) => pty_binary,
        None => st_runtime::resolve_executable("pty", &login_environment)?,
    };
    let state = AppState {
        store: store.clone(),
        notify: notify.clone(),
        event_notify: event_notify.clone(),
        node: config.node.clone(),
        state_dir: config.state_dir.clone(),
        pty_root: pty_root.clone(),
        pty_binary: pty_binary.clone(),
        fleet_id: config.fleet_id.clone(),
        configured_peers: config.peers.iter().map(|peer| peer.name.clone()).collect(),
        client_relay: st3::peer::ClientRelay::from_config(&config)?,
        native_session_home: std::env::var_os("HOME").map(PathBuf::from),
        planner_default: config.planner.clone(),
    };
    let reconciler = Arc::new(Reconciler::native(
        store.clone(),
        &config.state_dir,
        Some(&pty_root),
        &pty_binary,
        config.node.clone(),
        config.socket.display().to_string(),
        notify.clone(),
        event_notify.clone(),
    )?);
    tokio::spawn(reconciler.supervise());
    tokio::spawn(trim_local_observations(
        store.clone(),
        config.observations.clone(),
    ));
    if let Some(otlp) = &config.observations.otlp {
        let exporter = st3::otlp::OtlpExporter::new(otlp, &config.node)?;
        eprintln!(
            "st3: exporting local observations to OpenTelemetry at {}",
            otlp.endpoint
        );
        tokio::spawn(st3::otlp::run(store.clone(), exporter));
    }
    #[cfg(target_os = "macos")]
    tokio::spawn(async {
        // Startup and replication can leave large, empty malloc zones resident on macOS.
        // Give the system a chance to reclaim those pages without making request handling wait.
        loop {
            tokio::time::sleep(Duration::from_secs(120)).await;
            let _ = tokio::task::spawn_blocking(|| unsafe {
                malloc_zone_pressure_relief(std::ptr::null_mut(), 0)
            })
            .await;
        }
    });
    eprintln!("st: local API listening at {}", config.socket.display());
    eprintln!(
        "st: paired client gateway listening at {}",
        config.client_gateway_socket.display()
    );
    let local_socket = config.socket.clone();
    let client_gateway_socket = config.client_gateway_socket.clone();
    tokio::try_join!(
        st3::api::serve_unix_bound(&local_socket, router(state.clone())),
        serve_unix(&client_gateway_socket, fabric_router(state)),
    )?;
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
}

async fn run_launch(
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
            return print_value(&response, json_output);
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
            return print_value(&response, json_output);
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
    Ok(())
}

async fn run_mission_view(
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
        MissionViewCommand::Ls { all, cursor, limit } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the mission limit must be 1 through 200"
            );
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
            let selected = args.mission_or_run;
            let run = if selected.starts_with("mission-run/") {
                client
                    .get::<MissionRunView>(&format!(
                        "/v1/mission-runs/{}",
                        urlencoding::encode(&selected)
                    ))
                    .await?
            } else {
                let runs: Vec<MissionRunView> = client
                    .get(&format!(
                        "/v1/mission-runs?mission={}",
                        urlencoding::encode(&selected)
                    ))
                    .await?;
                anyhow::ensure!(
                    runs.len() == 1,
                    "mission `{selected}` has {} active runs; use an exact mission run subject",
                    runs.len()
                );
                runs.into_iter().next().expect("one active run was checked")
            };
            if args.follow {
                return follow_mission_run(client, run, 0, json_output).await;
            }
            if json_output {
                return print_value(&run, true);
            }
            let runs = load_mission_run_tree(client, &run).await?;
            print!(
                "{}",
                render_mission_run(&run, &runs, OutputStyle::stdout(), current_unix_ms()?)
            );
            Ok(())
        }
        MissionViewCommand::Publish(args) => publish_mission_file(client, args, json_output).await,
        MissionViewCommand::Start(args) => start_mission_run(client, args, json_output).await,
        MissionViewCommand::Cancel(args) => {
            cancel_mission_run(client, endpoint, args, json_output).await
        }
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

async fn publish_mission_file(
    client: &Client,
    args: MissionPublishArgs,
    json_output: bool,
) -> Result<()> {
    let (kdl, source_name) = read_intent(Some(&args.file))?;
    let intent = IntentInput { kdl, source_name };
    let mission: MissionResponse = client
        .post(
            "/v1/intent/mission",
            &MissionRequest {
                intent: intent.clone(),
                at_index: args.at_index,
            },
        )
        .await?;
    anyhow::ensure!(
        mission.blockers.is_empty(),
        "{}",
        mission.blockers.join("; ")
    );
    let resolved = mission.resolved_intent;
    let response: ApplyResponse = client
        .post(
            "/v1/intent/apply",
            &ApplyRequest {
                idempotency_key: idempotency(&resolved.kdl, &mission.subject_tokens),
                intent: resolved,
                expected_subjects: mission.subject_tokens,
                actor: Some(args.actor),
            },
        )
        .await?;
    // Name each exact revision so `missions start --revision` can require it on any host.
    let mut value = serde_json::to_value(&response)?;
    value["published_missions"] = mission
        .mission_revisions
        .iter()
        .map(|(subject, revision)| json!({"subject": subject, "revision": revision}))
        .collect();
    print_value(&value, json_output)
}

async fn cancel_mission_run(
    client: &Client,
    endpoint: &Endpoint,
    args: MissionCancelArgs,
    json_output: bool,
) -> Result<()> {
    let subject = normalize_member_subject(&args.mission_run, "mission-run");
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

async fn start_mission_run(
    client: &Client,
    args: MissionRunStartArgs,
    json_output: bool,
) -> Result<()> {
    let mission_id = args
        .mission
        .strip_prefix("mission/")
        .unwrap_or(&args.mission);
    let mission = startable_mission(
        client,
        mission_id,
        args.revision.as_deref(),
        MISSION_ARRIVAL_WAIT,
    )
    .await?;
    anyhow::ensure!(
        mission.state == MissionState::Ready,
        "mission `mission/{mission_id}` is not ready"
    );
    let run_id = args
        .id
        .unwrap_or_else(|| format!("{mission_id}/{}", uuid::Uuid::now_v7().simple()));
    let run_id = run_id.strip_prefix("mission-run/").unwrap_or(&run_id);
    let workspace = args
        .workspace
        .canonicalize()
        .with_context(|| format!("resolve workspace {}", args.workspace.display()))?;
    let inputs = unique_pairs(args.inputs, "input")?;
    let actor = args.actor;
    let requester = normalize_requester_subject(&actor);
    let after = args
        .after
        .map(|after| format!("mission-run/{}", after.trim_start_matches("mission-run/")));
    let kdl = mission_run_intent(
        run_id,
        mission_id,
        &mission.revision,
        &workspace,
        &requester,
        &inputs,
        "run",
        after.as_deref(),
    );
    if args.print_kdl {
        print!("{kdl}");
        return Ok(());
    }
    let subject = format!("mission-run/{run_id}");
    let response = publish_text(
        client,
        kdl,
        format!("st missions start {mission_id}"),
        actor,
    )
    .await
    .with_context(|| format!("start `{subject}` from mission `mission/{mission_id}`"))?;
    let started: MissionRunView = client
        .get(&format!(
            "/v1/mission-runs/{}",
            urlencoding::encode(&subject)
        ))
        .await?;
    let publications = mission_publications(client, mission_id).await?;
    let started_revision = started_revision_note(mission_id, &mission.revision, &publications);
    if !json_output {
        eprintln!("{started_revision}");
    }
    if !args.follow {
        return if json_output {
            print_value(
                &json!({
                    "publication": response,
                    "mission_run": started,
                    "mission_revision": mission.revision,
                    "started_revision": started_revision,
                }),
                true,
            )
        } else {
            println!("{}", started.subject);
            Ok(())
        };
    }
    follow_mission_run(client, started, response.store_index, json_output).await
}

/// How long `missions start` waits for a mission published on another host to arrive here.
const MISSION_ARRIVAL_WAIT: Duration = Duration::from_secs(60);

/// Read the mission that `missions start` will run. A publish on another host reaches this
/// host by replication, so a mission or requested revision that has not arrived yet waits
/// briefly with a plain message instead of failing.
async fn startable_mission(
    client: &Client,
    mission_id: &str,
    revision: Option<&str>,
    wait: Duration,
) -> Result<st3::model::MissionSpec> {
    let deadline = Instant::now() + wait;
    let mut announced = false;
    loop {
        let absent = match client
            .get::<st3::model::MissionSpec>(&format!(
                "/v1/missions/{}",
                urlencoding::encode(mission_id)
            ))
            .await
        {
            Ok(mission) if revision.is_none_or(|wanted| wanted == mission.revision) => {
                return Ok(mission);
            }
            Ok(mission) => {
                let wanted = revision.unwrap_or_default();
                let publications = mission_publications(client, mission_id).await?;
                anyhow::ensure!(
                    !publications
                        .iter()
                        .any(|publication| publication.revision == wanted),
                    "mission/{mission_id} revision {wanted} was replaced by revision {}. \
                     Start that revision, or publish again.",
                    mission.revision
                );
                format!("mission/{mission_id} revision {wanted} has not reached this host yet")
            }
            Err(error) if st3::client::is_not_found(&error) => {
                format!("mission/{mission_id} has not reached this host yet")
            }
            Err(error) => return Err(error),
        };
        let now = Instant::now();
        anyhow::ensure!(
            now < deadline,
            "{absent} after {}s. A mission published on another host arrives by replication; \
             check `st replication status`.",
            wait.as_secs()
        );
        if !announced {
            eprintln!(
                "{absent}. Waiting up to {}s for it to replicate here.",
                wait.as_secs()
            );
            announced = true;
        }
        tokio::time::sleep(Duration::from_millis(500).min(deadline - now)).await;
    }
}

struct MissionPublication {
    revision: String,
    origin: String,
    accepted_at_unix_ms: u128,
}

/// The mission's publications on this host, newest first.
async fn mission_publications(
    client: &Client,
    mission_id: &str,
) -> Result<Vec<MissionPublication>> {
    let page: ClaimsPage = client
        .get(&format!(
            "/v1/claims?subject={}&order=desc&limit=100",
            urlencoding::encode(&format!("mission/{mission_id}"))
        ))
        .await?;
    Ok(page
        .claims
        .into_iter()
        .filter(|claim| claim.kind == "mission.published")
        .filter_map(|claim| {
            Some(MissionPublication {
                revision: claim.body.get("revision")?.as_str()?.to_owned(),
                origin: claim.origin,
                accepted_at_unix_ms: claim.accepted_at_unix_ms,
            })
        })
        .collect())
}

/// Say which revision a run started, and whether other revisions share the mission name.
fn started_revision_note(
    mission_id: &str,
    revision: &str,
    publications: &[MissionPublication],
) -> String {
    let started = publications
        .iter()
        .find(|publication| publication.revision == revision);
    let others = publications
        .iter()
        .filter(|publication| publication.revision != revision)
        .map(|publication| publication.revision.as_str())
        .collect::<BTreeSet<_>>();
    let Some(started) = started.filter(|_| !others.is_empty()) else {
        return format!("Started mission/{mission_id} revision {revision}.");
    };
    let newer = publications
        .iter()
        .filter(|publication| {
            publication.revision != revision
                && publication.accepted_at_unix_ms > started.accepted_at_unix_ms
        })
        .count();
    let published = i64::try_from(started.accepted_at_unix_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "at an unknown time".into());
    let count = others.len();
    let plural = if count == 1 { "" } else { "s" };
    let relation = if newer == 0 { "older" } else { "other" };
    format!(
        "Started mission/{mission_id} revision {revision}, published {published} on {}. \
         {count} {relation} revision{plural} share{} this mission name.",
        started.origin,
        if count == 1 { "s" } else { "" }
    )
}

async fn follow_mission_run(
    client: &Client,
    mut run: MissionRunView,
    _cursor: u64,
    json_output: bool,
) -> Result<()> {
    let mut prior = String::new();
    let interactive = std::io::stdout().is_terminal();
    let _screen = if !json_output && interactive {
        Some(TerminalScreen::open()?)
    } else {
        None
    };
    let style = OutputStyle::stdout();
    loop {
        let runs = load_mission_run_tree(client, &run).await?;
        let summary = mission_run_signature(&runs)?;
        if summary != prior && !json_output {
            let frame = render_mission_run(&run, &runs, style, current_unix_ms()?);
            print!(
                "{}",
                follow_snapshot(&frame, interactive, !prior.is_empty())
            );
            std::io::stdout().flush()?;
            prior = summary;
        }
        match run.status.as_str() {
            status if mission_run_follow_succeeded(status) => {
                return if json_output {
                    print_value(&run, true)
                } else {
                    Ok(())
                };
            }
            "failed" | "cancelled" => {
                anyhow::bail!("mission run {} is {}", run.subject, run.status)
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        run = client
            .get(&format!(
                "/v1/mission-runs/{}",
                urlencoding::encode(&run.subject)
            ))
            .await?;
    }
}

async fn load_mission_run_tree(
    client: &Client,
    selected: &MissionRunView,
) -> Result<Vec<MissionRunView>> {
    let runs: Vec<MissionRunView> = client
        .get(&format!(
            "/v1/mission-runs?root={}",
            urlencoding::encode(&selected.root_mission_run)
        ))
        .await?;
    anyhow::ensure!(
        runs.iter().any(|run| run.subject == selected.subject),
        "mission run `{}` is absent from its root graph",
        selected.subject
    );
    Ok(runs)
}

fn mission_run_follow_succeeded(status: &str) -> bool {
    matches!(status, "completed" | "standing")
}

#[allow(clippy::too_many_arguments)]
fn mission_run_intent(
    run_id: &str,
    mission_id: &str,
    revision: &str,
    workspace: &Path,
    requester: &str,
    inputs: &BTreeMap<String, String>,
    mode: &str,
    after: Option<&str>,
) -> String {
    let mut run = KdlNode::new("mission-run");
    run.entries_mut().push(KdlEntry::new(run_id));
    let mut body = KdlDocument::new();
    let exact_mission = format!("mission/{mission_id}@{revision}");
    body.nodes_mut()
        .push(kdl_node("mission", [exact_mission.as_str()]));
    body.nodes_mut().push(kdl_node(
        "workspace",
        [workspace.to_string_lossy().as_ref()],
    ));
    body.nodes_mut().push(kdl_node("requester", [requester]));
    if mode != "run" {
        body.nodes_mut().push(kdl_node("mode", [mode]));
    }
    for (name, value) in inputs {
        body.nodes_mut()
            .push(kdl_node("input", [name.as_str(), value.as_str()]));
    }
    if let Some(after) = after {
        body.nodes_mut().push(kdl_node("after", [after]));
    }
    run.set_children(body);
    publication_document(run)
}

fn normalize_requester_subject(actor: &str) -> String {
    if actor.starts_with("person/") || actor.starts_with("agent/") {
        actor.to_owned()
    } else {
        format!("person/{actor}")
    }
}

fn kdl_node<'a>(name: &str, values: impl IntoIterator<Item = &'a str>) -> KdlNode {
    let mut node = KdlNode::new(name);
    node.entries_mut()
        .extend(values.into_iter().map(KdlEntry::new));
    node
}

fn publication_document(node: KdlNode) -> String {
    let mut document = KdlDocument::new();
    let mut version = KdlNode::new("version");
    version.entries_mut().push(KdlEntry::new(2));
    document.nodes_mut().push(version);
    document.nodes_mut().push(node);
    document.autoformat();
    document.to_string()
}

async fn publish_text(
    client: &Client,
    kdl: String,
    source_name: String,
    actor: String,
) -> Result<ApplyResponse> {
    let intent = IntentInput {
        kdl,
        source_name: Some(source_name),
    };
    let mission: MissionResponse = client
        .post(
            "/v1/intent/mission",
            &MissionRequest {
                intent: intent.clone(),
                at_index: None,
            },
        )
        .await?;
    anyhow::ensure!(
        mission.blockers.is_empty(),
        "{}",
        mission.blockers.join("; ")
    );
    let resolved = mission.resolved_intent;
    client
        .post(
            "/v1/intent/apply",
            &ApplyRequest {
                idempotency_key: idempotency(&resolved.kdl, &mission.subject_tokens),
                intent: resolved,
                expected_subjects: mission.subject_tokens,
                actor: Some(actor),
            },
        )
        .await
}

async fn run_pty(
    client: &Client,
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    command: PtyCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        PtyCommand::Ls { all, cursor, limit } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the terminal limit must be 1 through 200"
            );
            let response = generated_client(endpoint, None)?
                .terminals_list(cursor.as_deref(), Some(limit), all)
                .await?;
            let history = if all { " --all" } else { "" };
            print_product_page(
                "TERMINALS",
                &response,
                json_output,
                &format!("st terminals ls{history}"),
            )
        }
        PtyCommand::Attach(args) => {
            let subject = normalize_member_subject(&args.subject, "pty");
            attach_terminal(client, &subject, args.force).await
        }
        PtyCommand::Peek(args) => {
            let subject = normalize_member_subject(&args.subject, "pty");
            let screen: SessionScreen = client
                .get(&format!(
                    "/v1/sessions/screen/{}",
                    urlencoding::encode(&subject)
                ))
                .await?;
            if json_output {
                print_value(&screen, true)
            } else {
                print!("{}", screen.screen);
                Ok(())
            }
        }
        PtyCommand::Screen(args) => {
            let person = configured_human(
                args.person.as_deref(),
                configured_person,
                "terminals screen",
            )?;
            let response = generated_client(endpoint, Some(&person))?
                .terminal_screen(&args.subject)
                .await?;
            if json_output {
                print_value(&response, true)
            } else {
                print!("{}", render_terminal_screen(&response.value));
                Ok(())
            }
        }
        PtyCommand::AttachInfo(args) => {
            let person = configured_human(
                args.person.as_deref(),
                configured_person,
                "terminals attach-info",
            )?;
            let generated = generated_client(endpoint, Some(&person))?;
            let screen = generated.terminal_screen(&args.subject).await?;
            let capabilities = generated.capabilities().await?;
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let response = generated
                .terminal_attach(
                    format!("action/{nonce}"),
                    format!("terminal-attach:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        runtime_incarnation: Some(screen.value.runtime_incarnation),
                        terminal_sequence: Some(capabilities.snapshot.store_index),
                        ..ClientFence::default()
                    },
                    ClientTargetParameters {
                        target_id: screen.value.terminal_id,
                        ..ClientTargetParameters::default()
                    },
                )
                .await?;
            print_client_value(&response, json_output)
        }
        PtyCommand::Stream(args) => {
            let person = configured_human(
                args.person.as_deref(),
                configured_person,
                "terminals stream",
            )?;
            let mut stream = generated_client(endpoint, Some(&person))?
                .terminal_stream(&args.subject, args.incarnation.as_deref(), &args.capability)
                .await?;
            let mut shown = 0_u64;
            while args.count.is_none_or(|count| shown < count) {
                let Some(screen) = stream.next().await? else {
                    break;
                };
                shown += 1;
                if json_output {
                    println!("{}", serde_json::to_string(&screen)?);
                } else {
                    if shown > 1 {
                        println!();
                    }
                    print!("{}", render_terminal_screen(&screen.value));
                }
            }
            stream.close().await;
            Ok(())
        }
        PtyCommand::InputClient(args) => {
            let person = configured_human(
                args.person.as_deref(),
                configured_person,
                "terminals input-client",
            )?;
            let generated = generated_client(endpoint, Some(&person))?;
            let screen = generated.terminal_screen(&args.subject).await?;
            let capabilities = generated.capabilities().await?;
            let mode = if args.raw {
                ClientTerminalInputMode::Raw
            } else if args.key {
                ClientTerminalInputMode::Key
            } else {
                ClientTerminalInputMode::Line
            };
            let value = if args.raw {
                base64::engine::general_purpose::STANDARD.encode(args.value.as_bytes())
            } else {
                args.value
            };
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let response = generated
                .terminal_input(
                    format!("action/{nonce}"),
                    format!("terminal-input:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        runtime_incarnation: Some(screen.value.runtime_incarnation),
                        terminal_sequence: Some(screen.value.next_sequence),
                        ..ClientFence::default()
                    },
                    ClientTerminalInputParameters {
                        terminal_id: screen.value.terminal_id,
                        mode,
                        value,
                    },
                )
                .await?;
            print_client_value(&response, json_output)
        }
        PtyCommand::DetachClient(args) => {
            let person = configured_human(
                args.person.as_deref(),
                configured_person,
                "terminals detach-client",
            )?;
            let generated = generated_client(endpoint, Some(&person))?;
            let capabilities = generated.capabilities().await?;
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let response = generated
                .terminal_detach(
                    format!("action/{nonce}"),
                    format!("terminal-detach:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        runtime_incarnation: Some(args.incarnation),
                        ..ClientFence::default()
                    },
                    ClientTargetParameters {
                        target_id: args.attachment,
                        ..ClientTargetParameters::default()
                    },
                )
                .await?;
            print_client_value(&response, json_output)
        }
        PtyCommand::Send(args) => {
            let subject = normalize_member_subject(&args.subject, "pty");
            let incarnation = session_incarnation(client, &subject).await?;
            let mode = if args.raw {
                SessionInputMode::Raw
            } else if args.key {
                SessionInputMode::Key
            } else {
                SessionInputMode::Line
            };
            let value = if args.raw {
                base64::engine::general_purpose::STANDARD.encode(args.value.as_bytes())
            } else {
                args.value
            };
            let response: SessionControlResponse = client
                .post(
                    &format!("/v1/sessions/input/{}", urlencoding::encode(&subject)),
                    &SessionInputRequest {
                        expected_incarnation: incarnation,
                        mode,
                        value,
                        idempotency_key: format!("pty-input:{}:{}", subject, now_ms()),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
        PtyCommand::Signal(args) => {
            let subject = normalize_member_subject(&args.subject, "pty");
            let response: SessionControlResponse = client
                .post(
                    &format!("/v1/sessions/{}/signal", urlencoding::encode(&subject)),
                    &SessionSignalRequest {
                        expected_incarnation: session_incarnation(client, &subject).await?,
                        signal: args.signal,
                        idempotency_key: format!("pty-signal:{}:{}", subject, now_ms()),
                    },
                )
                .await?;
            print_value(&response, json_output)
        }
    }
}

fn render_terminal_screen(screen: &ClientTerminalScreen) -> String {
    let mut output = String::new();
    for line in &screen.lines {
        output.push_str(&line.text);
        output.push('\n');
    }
    output
}

async fn attach_terminal(client: &Client, subject: &str, force: bool) -> Result<()> {
    if !force
        && let Ok(outer) = std::env::var("PTY_SESSION")
        && !outer.is_empty()
    {
        anyhow::bail!(
            "st terminals attach: already inside PTY session `{outer}`. Detach first with Ctrl+\\, or pass --force."
        );
    }
    let attachment: Attachment = client
        .post(
            &format!("/v1/sessions/attach/{}", urlencoding::encode(subject)),
            &AttachRequest::default(),
        )
        .await?;
    let code = client
        .proxy_terminal_resilient(subject, &attachment)
        .await?;
    if code == 0 {
        Ok(())
    } else {
        Err(CommandExit(code.clamp(1, 255) as u8).into())
    }
}

async fn run_inspect(client: &Client, args: InspectArgs, json_output: bool) -> Result<()> {
    if args.subject.starts_with("resource/")
        && let Some((subject, claim_id)) = args.subject.rsplit_once('@')
    {
        let claim: ClaimRecord = client
            .get(&format!(
                "/v1/claims/by-id/{}",
                urlencoding::encode(claim_id)
            ))
            .await?;
        anyhow::ensure!(
            claim.subject == subject,
            "claim `{claim_id}` belongs to `{}`, not `{subject}`",
            claim.subject
        );
        return print_value(
            &json!({
                "reference": args.subject,
                "actual": claim.body,
                "claim": claim,
            }),
            json_output,
        );
    }
    let status = status_for(client, &args.subject).await?;
    let claims: ClaimsPage = client
        .get(&format!(
            "/v1/claims?subject={}&order=desc&limit=20",
            urlencoding::encode(&args.subject)
        ))
        .await?;
    print_value(
        &json!({ "status": status, "recent_claims": claims.claims }),
        json_output,
    )
}

async fn run_trace(client: &Client, args: TraceArgs, json_output: bool) -> Result<()> {
    anyhow::ensure!(
        args.limit > 0 && args.limit <= 500,
        "the trace limit must be 1 through 500"
    );
    let claims = trace_claims(client, &args).await?;
    let mut cursor = args.after_index.unwrap_or_default();
    for claim in claims {
        cursor = cursor.max(claim.store_index);
        if json_output {
            println!("{}", serde_json::to_string(&claim)?);
        } else {
            print_trace_claim(&claim);
        }
    }
    if !args.follow {
        return Ok(());
    }
    loop {
        let mut event_query = vec![format!("after={cursor}")];
        if let Some(subject) = &args.subject {
            event_query.push(format!("subject={}", urlencoding::encode(subject)));
        }
        if let Some(owner_run) = &args.owner_run {
            event_query.push(format!("owner_run={}", urlencoding::encode(owner_run)));
        }
        let events: Vec<EventRecord> = client
            .get(&format!("/v1/events?{}", event_query.join("&")))
            .await?;
        for event in events {
            cursor = cursor.max(event.store_index);
            if json_output {
                println!("{}", serde_json::to_string(&event)?);
            } else {
                let claims: ClaimsPage = client
                    .get(&format!(
                        "/v1/claims?subject={}&after_index={}&order=asc&limit=1",
                        urlencoding::encode(&event.subject),
                        event.store_index.saturating_sub(1)
                    ))
                    .await?;
                if let Some(claim) = claims
                    .claims
                    .into_iter()
                    .find(|claim| claim.store_index == event.store_index)
                {
                    print_trace_claim(&claim);
                } else {
                    println!(
                        "{}\t{}\t{}\t(no claim details)",
                        event.store_index, event.kind, event.subject
                    );
                }
            }
        }
    }
}

async fn trace_claims(client: &Client, args: &TraceArgs) -> Result<Vec<ClaimRecord>> {
    let order = if args.after_index.is_some() {
        "asc"
    } else {
        "desc"
    };
    let mut query = vec![format!("limit={}", args.limit), format!("order={order}")];
    if let Some(subject) = &args.subject {
        query.push(format!("subject={}", urlencoding::encode(subject)));
    }
    if let Some(owner_run) = &args.owner_run {
        query.push(format!("owner_run={}", urlencoding::encode(owner_run)));
    }
    if let Some(after) = args.after_index {
        query.push(format!("after_index={after}"));
    }
    let page: ClaimsPage = client
        .get(&format!("/v1/claims?{}", query.join("&")))
        .await?;
    let mut claims = page.claims;
    if args.after_index.is_none() {
        claims.reverse();
    }
    Ok(claims)
}

fn print_trace_claim(claim: &ClaimRecord) {
    let fields = claim.body.get("fields").unwrap_or(&claim.body);
    let summary = ["state", "status", "verdict", "action", "reason"]
        .into_iter()
        .filter_map(|key| {
            fields
                .get(key)
                .filter(|value| !value.is_null())
                .map(|value| format!("{key}={}", trace_scalar(value)))
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let timestamp = chrono::DateTime::from_timestamp_millis(
        claim.accepted_at_unix_ms.min(i64::MAX as u128) as i64,
    )
    .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    .unwrap_or_else(|| claim.accepted_at_unix_ms.to_string());
    if summary.is_empty() {
        println!(
            "{}\t{}\t{}\t{}",
            claim.store_index, timestamp, claim.kind, claim.subject
        );
    } else {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            claim.store_index, timestamp, claim.kind, claim.subject, summary
        );
    }
}

fn trace_scalar(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

async fn run_wait(client: &Client, args: WaitArgs, json_output: bool) -> Result<()> {
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

fn generated_client(endpoint: &Endpoint, person: Option<&str>) -> Result<GeneratedClient> {
    let Endpoint::Unix(socket) = endpoint else {
        anyhow::bail!(
            "client-v0 product commands require the trusted local Unix endpoint; remote clients must use a paired Fabric credential"
        );
    };
    Ok(person
        .map_or_else(
            || GeneratedClient::unix(socket),
            |person| GeneratedClient::unix_as(socket, person),
        )
        .with_outage_wait(DAEMON_WAIT.get().copied().unwrap_or_default(), true))
}

async fn run_now(
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    args: NowArgs,
    json_output: bool,
) -> Result<()> {
    anyhow::ensure!(
        args.limit > 0 && args.limit <= 200,
        "the now limit must be 1 through 200"
    );
    let person = args.person.as_deref().or(configured_person).context(
        "st now needs `--as person/NAME` or `person = \"person/NAME\"` in the st config",
    )?;
    let person = parse_person_subject(person).map_err(anyhow::Error::msg)?;
    let client = generated_client(endpoint, Some(&person))?;
    let response = if let Some(owner_run) = args.owner_run.as_deref() {
        client
            .now_list_for_owner_run(
                owner_run,
                args.cursor.as_deref(),
                Some(args.limit),
                args.all,
            )
            .await?
    } else {
        client
            .now_list(args.cursor.as_deref(), Some(args.limit), args.all)
            .await?
    };
    let mut command = format!("st now --as {person}");
    if let Some(owner_run) = args.owner_run {
        command.push_str(&format!(" --owner-run {owner_run}"));
    }
    if args.all {
        command.push_str(" --all");
    }
    if json_output {
        print_value(&response, true)
    } else {
        print!("{}", render_now_page(&response.value, &command));
        Ok(())
    }
}

fn render_now_page(page: &ClientPage, continuation_command: &str) -> String {
    let mut needs_you = page.clone();
    needs_you
        .items
        .retain(|item| matches!(item, ClientResource::Attention(_)));
    needs_you.page.next_cursor = None;
    let mut unhealthy = page.clone();
    unhealthy.items.retain(|item| match item {
        ClientResource::Operation(_) => true,
        ClientResource::Agent(agent) => {
            agent.reachability != "reachable"
                || matches!(agent.state.as_str(), "failed" | "waiting" | "stopped")
        }
        ClientResource::Runtime(runtime) => runtime.state != "running",
        _ => false,
    });
    unhealthy.page.next_cursor = None;
    let mut working = page.clone();
    working.items.retain(|item| {
        !matches!(
            item,
            ClientResource::Attention(_) | ClientResource::Operation(_)
        ) && !matches!(item, ClientResource::Agent(agent)
                if agent.reachability != "reachable"
                    || matches!(agent.state.as_str(), "failed" | "waiting" | "stopped"))
            && !matches!(item, ClientResource::Runtime(runtime) if runtime.state != "running")
    });
    working.page.next_cursor = None;
    let mut output = String::new();
    if let Some(sync) = &page.sync {
        output.push_str(&render_sync_notice(sync, now_ms()));
    }
    output.push_str(&render_product_page(
        "NEEDS YOU",
        &needs_you,
        continuation_command,
    ));
    // The server fills Now with attention, and adds work only for an explicit work
    // filter. Print a section only when the page holds its items, so a section the
    // server never filled does not read as zero.
    for (title, section) in [("WORKING", &working), ("UNHEALTHY", &unhealthy)] {
        if !section.items.is_empty() {
            output.push('\n');
            output.push_str(&render_product_page(title, section, continuation_command));
        }
    }
    if working.items.is_empty() && unhealthy.items.is_empty() {
        output.push_str("\nWork: st work ls · Health: st doctor\n");
    }
    if let Some(cursor) = page.page.next_cursor.as_deref() {
        use std::fmt::Write as _;
        let _ = writeln!(
            output,
            "More items are available: {continuation_command} --cursor {cursor} --limit {}",
            page.page.limit
        );
    }
    output
}

async fn run_machines(endpoint: &Endpoint, args: MachinesArgs, json_output: bool) -> Result<()> {
    anyhow::ensure!(
        args.limit > 0 && args.limit <= 200,
        "the machine limit must be 1 through 200"
    );
    let response = generated_client(endpoint, None)?
        .machines_list(args.cursor.as_deref(), Some(args.limit), args.all)
        .await?;
    let history = if args.all { " --all" } else { "" };
    print_product_page(
        "MACHINES",
        &response,
        json_output,
        &format!("st machines{history}"),
    )
}

async fn run_activity(endpoint: &Endpoint, args: ActivityArgs, json_output: bool) -> Result<()> {
    anyhow::ensure!(
        args.limit > 0 && args.limit <= 500,
        "the activity limit must be 1 through 500"
    );
    let client = generated_client(endpoint, None)?;
    let mut cursor = args.after;
    loop {
        let response = client
            .events(
                cursor.as_deref(),
                Some(args.limit),
                args.follow.then_some(30_000),
            )
            .await?;
        cursor = Some(response.value.resume_cursor.clone());
        print_activity_page(&response, json_output, args.all)?;
        if !args.follow {
            return Ok(());
        }
    }
}

async fn run_devices(
    endpoint: Endpoint,
    configured_person: Option<&str>,
    args: DevicesArgs,
    json_output: bool,
) -> Result<()> {
    let DevicesArgs {
        person,
        all,
        cursor,
        limit,
        command,
    } = args;
    let person = configured_human(person.as_deref(), configured_person, "devices")?;
    let client = generated_client(&endpoint, Some(&person))?;
    match command.unwrap_or(DevicesCommand::Ls) {
        DevicesCommand::Ls => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the device limit must be 1 through 200"
            );
            let response = client
                .devices_list(cursor.as_deref(), Some(limit), all)
                .await?;
            let history = if all { " --all" } else { "" };
            print_product_page(
                "DEVICES",
                &response,
                json_output,
                &format!("st devices --as {person}{history}"),
            )
        }
        DevicesCommand::Pair {
            device_name,
            full_control,
        } => {
            let response = client
                .pairing_begin(&PairingBegin {
                    api_version: CLIENT_V0_API_VERSION.into(),
                    device_name,
                    person_id: person,
                    full_control: full_control.then_some(true),
                })
                .await?;
            print_client_value(&response, json_output)
        }
        DevicesCommand::Revoke { device, reason } => {
            let capabilities = client.capabilities().await?;
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let response = client
                .pairing_revoke(
                    format!("action/{nonce}"),
                    format!("pairing-revoke:{device}:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        ..ClientFence::default()
                    },
                    ClientTargetParameters {
                        target_id: device,
                        reason,
                        ..ClientTargetParameters::default()
                    },
                )
                .await?;
            print_client_value(&response, json_output)
        }
    }
}

fn configured_human(
    explicit: Option<&str>,
    configured: Option<&str>,
    command: &str,
) -> Result<String> {
    let person = explicit.or(configured).with_context(|| {
        format!(
            "st {command} needs `--as person/NAME` or `person = \"person/NAME\"` in the st config"
        )
    })?;
    parse_person_subject(person).map_err(anyhow::Error::msg)
}

fn print_client_value<T: serde::Serialize>(
    response: &ClientEnvelope<T>,
    json_output: bool,
) -> Result<()> {
    if json_output {
        print_value(response, true)
    } else {
        print_value(&response.value, false)
    }
}

fn print_product_page(
    title: &str,
    response: &ClientEnvelope<ClientPage>,
    json_output: bool,
    continuation_command: &str,
) -> Result<()> {
    if json_output {
        return print_value(response, true);
    }
    if let Some(sync) = &response.value.sync {
        print!("{}", render_sync_notice(sync, now_ms()));
    }
    print!(
        "{}",
        render_product_page(title, &response.value, continuation_command)
    );
    Ok(())
}

/// A host catching up with a peer shows early history as current, so say so before the items.
fn render_sync_notice(sync: &st3_client::SyncNotice, now: u128) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    for peer in &sync.peers {
        let last_exchange = peer
            .last_exchange_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| {
                format!(
                    " · last exchange {}",
                    relative_time(at.timestamp_millis().max(0) as u128, now)
                )
            })
            .unwrap_or_default();
        let _ = writeln!(output, "SYNCING  {}{last_exchange}", peer.summary());
    }
    let _ = writeln!(
        output,
        "  Until then, items below can be out of date. Progress: st3 replication status\n"
    );
    output
}

/// `target mission/fleet/typecase: cancelled 4h ago`
fn attention_target_line(target: &st3_client::AttentionTargetState, now_unix_ms: u128) -> String {
    let since = target
        .since
        .as_deref()
        .and_then(|since| chrono::DateTime::parse_from_rfc3339(since).ok())
        .map(|since| {
            format!(
                " {}",
                relative_time(since.timestamp_millis().max(0) as u128, now_unix_ms)
            )
        })
        .unwrap_or_default();
    format!("target {}: {}{since}", target.id, target.state)
}

/// Active and finished runs apart, so a mission with one live run and five old ones does not
/// read as six runs. A daemon that does not report active runs gets the plain total.
fn render_mission_runs(mission: &st3_client::Mission) -> String {
    let total = mission.runs.len();
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    let Some(active) = mission.active_runs.map(|active| active.min(total)) else {
        return format!("{total} run{}", plural(total));
    };
    match (active, total - active) {
        (0, 0) => "0 runs".into(),
        (active, 0) => format!("{active} active run{}", plural(active)),
        (0, finished) => format!("{finished} finished run{}", plural(finished)),
        (active, finished) => format!("{active} active · {finished} finished"),
    }
}

fn render_product_page(title: &str, page: &ClientPage, continuation_command: &str) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "{title}  {}", page.items.len());
    if !page.filters.is_empty() {
        let filters = page
            .filters
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" · ");
        let _ = writeln!(output, "FILTERS  {filters}");
    }
    if page.items.is_empty() {
        let _ = writeln!(output, "No current items.");
        return output;
    }
    for item in &page.items {
        match item {
            ClientResource::Attention(item) => {
                let _ = writeln!(
                    output,
                    "{}  attention  {}  {}  {}",
                    item.header.id, item.priority, item.state, item.title
                );
                for target in &item.target_states {
                    let _ = writeln!(output, "  {}", attention_target_line(target, now_ms()));
                }
                let _ = writeln!(
                    output,
                    "  action: st attention show {} --as {}",
                    item.source_id, item.person_id
                );
                if item.header.operational.as_ref().is_some_and(|operational| {
                    operational
                        .reasons
                        .iter()
                        .any(|reason| reason == "requester-retired")
                }) {
                    let _ = writeln!(
                        output,
                        "  requester retired: only {} can close it",
                        item.person_id
                    );
                }
            }
            ClientResource::Work(item) => {
                let _ = writeln!(
                    output,
                    "{}  work  {}  {}  attempt {}",
                    item.header.id, item.state, item.path, item.attempt
                );
                if let Some(claimant) = &item.claimant {
                    let _ = writeln!(output, "  assigned: {claimant}");
                }
                let _ = writeln!(output, "  action: st work show {}", item.header.id);
            }
            ClientResource::Mission(item) => {
                let _ = writeln!(
                    output,
                    "{}  {}  {}",
                    item.header.id,
                    item.state,
                    render_mission_runs(item)
                );
                if let Some(usage) = &item.usage {
                    let _ = writeln!(output, "  usage {}", render_usage(usage));
                }
                if let Some(run) = item.runs.last() {
                    let _ = writeln!(output, "  inspect: st missions show {run}");
                }
            }
            ClientResource::Launch(item) => {
                let _ = writeln!(
                    output,
                    "{}  {}  {} variant{} · {} decision{}",
                    item.header.id,
                    item.phase,
                    item.variants.len(),
                    if item.variants.len() == 1 { "" } else { "s" },
                    item.decisions.len(),
                    if item.decisions.len() == 1 { "" } else { "s" }
                );
                let _ = writeln!(output, "  {}", item.title);
                let _ = writeln!(output, "  inspect: st launch show {}", item.header.id);
            }
            ClientResource::Operation(item) => {
                let _ = writeln!(
                    output,
                    "{}  operation  {}  {}  {}",
                    item.header.id, item.severity, item.state, item.summary
                );
                let _ = writeln!(output, "  recovery: st doctor");
            }
            ClientResource::Agent(item) => {
                let _ = writeln!(
                    output,
                    "{}  {}  {} · {}",
                    item.header.id, item.state, item.name, item.reachability
                );
                if let Some(owner) = &item.owner_run_id {
                    let _ = writeln!(output, "  owner: {owner}");
                }
            }
            ClientResource::Runtime(item) => {
                let _ = writeln!(
                    output,
                    "{}  {}  {} · {}",
                    item.header.id, item.state, item.owner_id, item.owner_host_id
                );
                let _ = writeln!(output, "  runtime: {}", item.runtime_id);
                if item.terminal_id.is_some() && item.state == "running" {
                    let _ = writeln!(output, "  peek: st terminals peek {}", item.owner_id);
                    let _ = writeln!(output, "  attach: st terminals attach {}", item.owner_id);
                }
                if let Some(owner) = &item.owner_run_id {
                    let _ = writeln!(output, "  owner: {owner}");
                }
            }
            ClientResource::Machine(item) => {
                let _ = writeln!(output, "{}  {}", item.header.id, item.state);
                if item.capacity.state == "unknown" {
                    let _ = writeln!(output, "  capacity not reported");
                } else {
                    let _ = writeln!(
                        output,
                        "  capacity {} — {}",
                        item.capacity.state, item.capacity.reason
                    );
                }
                let _ = writeln!(
                    output,
                    "  runtimes {} running · {} known",
                    item.occupancy.running_runtimes,
                    item.runtime_ids.len()
                );
                let _ = writeln!(output, "  assigned work {}", item.work.len());
                if !item.projects.is_empty() {
                    let _ = writeln!(output, "  projects {}", item.projects.len());
                }
                for transport in &item.transports {
                    let _ = write!(
                        output,
                        "  transport {} {}",
                        transport.protocol, transport.status
                    );
                    if let Some(last_success_at) = &transport.last_success_at {
                        let _ = write!(output, " · last success {last_success_at}");
                    }
                    let _ = writeln!(output);
                }
                let _ = writeln!(output, "  inspect: st subject show {}", item.host_id);
                if !matches!(item.state.as_str(), "local" | "reachable") {
                    let _ = writeln!(output, "  recovery: st replication status");
                }
            }
            ClientResource::Device(item) => {
                let _ = writeln!(
                    output,
                    "{}  {}  {}  scopes {}",
                    item.header.id,
                    item.state,
                    item.session_actor,
                    item.scopes.len()
                );
                let _ = writeln!(
                    output,
                    "  action: st devices --as {} revoke {}",
                    item.person_id, item.header.id
                );
            }
            ClientResource::Session(item) => {
                let _ = writeln!(
                    output,
                    "{}  {}  {} · started {}",
                    item.header.id, item.state, item.owner_id, item.started_at
                );
                if let Some(usage) = &item.usage {
                    let _ = writeln!(output, "  usage {}", render_usage(usage));
                }
                let _ = writeln!(
                    output,
                    "  timeline: st conversations timeline {}",
                    item.header.id
                );
            }
            item => {
                let _ = writeln!(output, "{}  resource", item.header().id);
            }
        }
    }
    if let Some(cursor) = page.page.next_cursor.as_deref() {
        let _ = writeln!(
            output,
            "More items are available: {continuation_command} --cursor {cursor} --limit {}",
            page.page.limit
        );
    }
    output
}

/// A mailbox listing with the same heading, filters, and empty line as the other lists. Each row
/// stays one tab-separated message.
fn render_mailbox(identity: &str, sender: Option<&str>, archive: bool, rows: &[String]) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "MESSAGES  {}", rows.len());
    let mut filters = vec![format!("mailbox={identity}")];
    if let Some(sender) = sender {
        filters.push(format!("from={sender}"));
    }
    if archive {
        filters.push("archived=included".into());
    }
    let _ = writeln!(output, "FILTERS  {}", filters.join(" · "));
    if rows.is_empty() {
        let _ = writeln!(output, "No current items.");
    }
    for row in rows {
        let _ = writeln!(output, "{row}");
    }
    output
}

/// Token spend, or that none was reported. Some drivers report only context occupancy, which
/// counts no spend, so a summary without a spending incarnation is unknown, not zero.
fn render_usage(usage: &st3_client::UsageSummary) -> String {
    if usage.incarnation_count > 0 {
        return format!("{} tokens", usage.total_tokens);
    }
    match usage
        .context
        .as_ref()
        .and_then(|context| context.used_tokens)
    {
        Some(used) => format!("not reported · context {used} tokens"),
        None => "not reported".into(),
    }
}

fn print_activity_page(
    response: &ClientEnvelope<ClientEventPage>,
    json_output: bool,
    all: bool,
) -> Result<()> {
    if json_output {
        return print_value(response, true);
    }
    print!("{}", render_activity_page_with_all(&response.value, all));
    Ok(())
}

#[cfg(test)]
fn render_activity_page(page: &ClientEventPage) -> String {
    render_activity_page_with_all(page, false)
}

fn render_activity_page_with_all(page: &ClientEventPage, all: bool) -> String {
    use std::fmt::Write as _;

    let items = page
        .items
        .iter()
        .filter(|item| {
            all || !matches!(
                item.body.get("change").and_then(Value::as_str),
                Some("work.renewed" | "replication.heartbeat")
            )
        })
        .collect::<Vec<_>>();
    let mut output = String::new();
    let _ = writeln!(output, "ACTIVITY  {}", items.len());
    if items.is_empty() {
        let _ = writeln!(
            output,
            "No material changes. Resume after {}.",
            page.resume_cursor
        );
    }
    for item in items {
        let resources = if item.resource_ids.is_empty() {
            item.body
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned()
        } else {
            item.resource_ids.join(",")
        };
        let change = item
            .body
            .get("change")
            .and_then(Value::as_str)
            .unwrap_or_else(|| client_event_type_label(&item.event_type));
        let state = item
            .body
            .get("state")
            .and_then(Value::as_str)
            .map(|state| format!(" → {state}"))
            .unwrap_or_default();
        let _ = writeln!(
            output,
            "{}  {}{}  {}  cursor {}",
            resources, change, state, item.timestamp, item.next_cursor
        );
    }
    if page.has_more {
        let _ = writeln!(
            output,
            "More changes are available after {}.",
            page.resume_cursor
        );
    }
    output
}

fn client_event_type_label(event_type: &ClientEventType) -> &'static str {
    match event_type {
        ClientEventType::Upsert => "upsert",
        ClientEventType::Delete => "delete",
        ClientEventType::TimelineDelta => "timeline.delta",
        ClientEventType::TerminalAvailable => "terminal.available",
        ClientEventType::CapabilitiesChanged => "capabilities.changed",
        ClientEventType::Unknown => "unknown",
    }
}

fn print_timeline_page(
    response: &ClientEnvelope<ClientTimelinePage>,
    json_output: bool,
) -> Result<()> {
    if json_output {
        return print_value(response, true);
    }
    use std::fmt::Write as _;
    let mut output = String::new();
    let _ = writeln!(
        output,
        "CONVERSATION  {} · {} entries",
        response.value.session_id,
        response.value.items.len()
    );
    if response.value.items.is_empty() {
        let _ = writeln!(output, "No normalized timeline entries.");
    }
    for entry in &response.value.items {
        let kind = format!("{:?}", entry.body.entry_type()).to_lowercase();
        let role = format!("{:?}", entry.role).to_lowercase();
        let _ = writeln!(
            output,
            "\n#{}  {} · {} · {}",
            entry.sequence, entry.timestamp, role, kind
        );
        match &entry.body {
            ClientTimelineBody::Message(body) => {
                let _ = write!(output, "message {}", body.message_id);
                if let Some(reply_to) = &body.reply_to {
                    let _ = write!(output, " · reply to {reply_to}");
                }
                let _ = writeln!(output);
            }
            ClientTimelineBody::Content(body) => {
                if let Some(text) = &body.text {
                    let _ = writeln!(output, "{text}");
                } else if let Some(attachment) = &body.attachment_id {
                    let _ = writeln!(output, "attachment {attachment} ({})", body.media_type);
                }
            }
            ClientTimelineBody::ToolCall(body) => {
                let _ = writeln!(output, "tool {} ({})", body.name, body.call_id);
                let _ = writeln!(output, "{}", body.arguments);
            }
            ClientTimelineBody::ToolResult(body) => {
                let _ = writeln!(output, "tool result {} · {:?}", body.call_id, body.status);
                let _ = writeln!(output, "{}", body.content);
            }
            ClientTimelineBody::Status(body) => {
                let _ = writeln!(output, "{:?}", body.status);
                if let Some(detail) = &body.detail {
                    let _ = writeln!(output, "{detail}");
                }
            }
            ClientTimelineBody::Error(body) => {
                let _ = writeln!(output, "{}: {}", body.code, body.message);
            }
            ClientTimelineBody::Usage(body) => {
                let _ = writeln!(
                    output,
                    "{} tokens · {} · {:?}",
                    body.total_tokens.unwrap_or_default(),
                    body.driver,
                    body.semantics
                );
            }
            ClientTimelineBody::Redaction(body) => {
                let _ = writeln!(
                    output,
                    "redacted {} bytes: {}",
                    body.withheld_bytes, body.reason
                );
            }
            ClientTimelineBody::Truncation(body) => {
                let _ = writeln!(
                    output,
                    "omitted sequences {}..{}: {}",
                    body.omitted_from_sequence, body.omitted_to_sequence, body.reason
                );
            }
            ClientTimelineBody::Unknown { entry_type, body } => {
                let _ = writeln!(output, "unrecognized {entry_type} timeline entry");
                let _ = writeln!(output, "{body}");
            }
        }
    }
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

fn unseen_timeline_entries(
    entries: &[ClientTimelineEntry],
    seen: &mut BTreeMap<String, (u32, u64)>,
) -> Vec<ClientTimelineEntry> {
    let mut changed = Vec::new();
    for entry in entries {
        if seen
            .get(&entry.id)
            .is_none_or(|(revision, _)| *revision < entry.revision)
        {
            seen.insert(entry.id.clone(), (entry.revision, entry.sequence));
            changed.push(entry.clone());
        }
    }
    if seen.len() > 4_096 {
        let mut oldest = seen
            .iter()
            .map(|(id, (_, sequence))| (id.clone(), *sequence))
            .collect::<Vec<_>>();
        oldest.sort_by_key(|(_, sequence)| *sequence);
        for (id, _) in oldest.into_iter().take(seen.len() - 4_096) {
            seen.remove(&id);
        }
    }
    changed.sort_by_key(|entry| entry.sequence);
    changed
}

fn print_follow_entries(
    response: &ClientEnvelope<ClientTimelinePage>,
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
    let mut delta = response.clone();
    delta.value.items = changed;
    delta.value.page.has_more = false;
    delta.value.page.next_cursor = None;
    print_timeline_page(&delta, false)
}

async fn follow_conversation(
    client: &GeneratedClient,
    session: &str,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    // Subscribe before reading the first page so a change during that read cannot be lost.
    let mut event_cursor = client.capabilities().await?.value.event_cursor;
    let mut seen = BTreeMap::new();
    let initial = client.timeline(session, None, Some(limit)).await?;
    print_follow_entries(
        &initial,
        unseen_timeline_entries(&initial.value.items, &mut seen),
        json_output,
    )?;
    let mut last_read = Instant::now();
    loop {
        let events = match client
            .events(Some(&event_cursor), Some(200), Some(3_000))
            .await
        {
            Ok(events) => events,
            Err(GeneratedClientError::Api(ClientErrorCode::CursorGap, _, _)) => {
                // Keep the visible window; re-establish the subscription and compare revisions.
                event_cursor = client.capabilities().await?.value.event_cursor;
                let page = client.timeline(session, None, Some(limit)).await?;
                print_follow_entries(
                    &page,
                    unseen_timeline_entries(&page.value.items, &mut seen),
                    json_output,
                )?;
                last_read = Instant::now();
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        event_cursor = events.value.resume_cursor;
        let relevant = events
            .value
            .items
            .iter()
            .any(|event| event.resource_ids.iter().any(|id| id == session));
        // Native host-local transcripts may advance without a graph event. This bounded
        // fallback runs only while this explicit follow command is active.
        if relevant || last_read.elapsed() >= Duration::from_secs(3) {
            let page = client.timeline(session, None, Some(limit)).await?;
            print_follow_entries(
                &page,
                unseen_timeline_entries(&page.value.items, &mut seen),
                json_output,
            )?;
            last_read = Instant::now();
        }
    }
}

async fn run_subject(client: &Client, command: SubjectCommand, json_output: bool) -> Result<()> {
    match command {
        SubjectCommand::Show(args) => run_inspect(client, args, json_output).await,
        SubjectCommand::History(args) => run_trace(client, args, json_output).await,
    }
}

async fn run_trace_command(
    client: &Client,
    command: TraceCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        TraceCommand::Show(args) => run_trace(client, args, json_output).await,
        TraceCommand::Wait(args) => run_wait(client, args, json_output).await,
    }
}

async fn wait_for_condition(
    client: &Client,
    subject: &str,
    condition: &str,
    actor: Option<&str>,
) -> Result<Value> {
    let mut cursor = 0;
    loop {
        if let Some(value) = condition_value(client, subject, condition).await? {
            return Ok(value);
        }
        if let Some(actor) = actor
            && let Some(reason) = agent_wait_interruption(client, actor).await?
        {
            anyhow::bail!(reason);
        }
        let scope = actor.map_or_else(
            || format!("&subject={}", urlencoding::encode(subject)),
            |_| String::new(),
        );
        let events: Vec<EventRecord> = client
            .get(&format!(
                "/v1/events?after={cursor}{scope}&wait=true&timeout_ms=30000"
            ))
            .await?;
        for event in events {
            cursor = cursor.max(event.store_index);
        }
    }
}

async fn agent_wait_interruption(client: &Client, actor: &str) -> Result<Option<String>> {
    let work: Vec<StepRunView> = client
        .get(&format!(
            "/v1/work?actor={}&include_terminal=false",
            urlencoding::encode(actor)
        ))
        .await?;
    let ready = work
        .iter()
        .filter(|step| step.status == "ready")
        .map(|step| step.subject.clone())
        .collect::<Vec<_>>();
    let has_claimed_work = work.iter().any(|step| {
        matches!(step.status.as_str(), "claimed" | "working")
            && step.claimant.as_deref() == Some(actor)
    });
    let mut unread = Vec::new();
    for_each_message(client, Some(actor), false, |message| {
        if matches!(message.status.as_str(), "sent" | "delivered") {
            unread.push(message.subject);
        }
        Ok(())
    })
    .await?;
    Ok(wait_interruption_reason(
        actor,
        has_claimed_work,
        &ready,
        &unread,
    ))
}

fn wait_interruption_reason(
    actor: &str,
    has_claimed_work: bool,
    ready: &[String],
    unread: &[String],
) -> Option<String> {
    if !ready.is_empty() {
        return Some(format!(
            "the wait stopped because {actor} has ready work: {}. Run `st work ls`",
            ready.join(", ")
        ));
    }
    if !unread.is_empty() {
        return Some(format!(
            "the wait stopped because {actor} has a new message: {}. Run `st conversations ls`",
            unread.join(", ")
        ));
    }
    (!has_claimed_work).then(|| {
        format!(
            "{actor} cannot wait without claimed work. Finish this turn and let native delivery start the next turn"
        )
    })
}

async fn condition_value(client: &Client, subject: &str, condition: &str) -> Result<Option<Value>> {
    if let Some(expected) = condition.strip_prefix("verdict=") {
        let eval: EvalStatus = client
            .get(&format!("/v1/evals/{}", urlencoding::encode(subject)))
            .await?;
        return Ok((eval.verdict.as_deref() == Some(expected)).then(|| json!(eval)));
    }
    let status = status_for(client, subject).await?;
    let matches = st3::model::status_wait_condition_holds(condition, status.subjects.first());
    Ok(matches.then(|| json!(status)))
}

fn validate_wait_condition(condition: &str) -> Result<()> {
    anyhow::ensure!(
        st3::model::STATUS_WAIT_CONDITIONS.contains(&condition)
            || matches!(
                condition.strip_prefix("verdict="),
                Some("pass" | "fail" | "void")
            ),
        "unknown wait condition `{condition}`"
    );
    Ok(())
}

fn parse_timeout(value: &str) -> Result<Duration> {
    if value == "0" {
        return Ok(Duration::ZERO);
    }
    for (suffix, factor) in [("ms", 1_u64), ("s", 1_000), ("m", 60_000), ("h", 3_600_000)] {
        if let Some(number) = value.strip_suffix(suffix) {
            let amount = number.parse::<u64>()?;
            anyhow::ensure!(amount > 0, "a timeout must be positive or zero");
            return Ok(Duration::from_millis(amount.saturating_mul(factor)));
        }
    }
    anyhow::bail!("a timeout must use ms, s, m, h, or zero")
}

async fn run_doctor(client: &Client, args: DoctorArgs, json_output: bool) -> Result<()> {
    let report: DoctorReport = client.get("/v1/doctor").await?;
    if json_output {
        print_value(&report, true)?;
    } else {
        for check in &report.checks {
            println!("{}\t{}\t{}", check.status, check.name, check.message);
        }
    }
    anyhow::ensure!(report.status != "fail", "st doctor found a failed check");
    anyhow::ensure!(
        !args.strict || report.status == "pass",
        "st doctor found a warning in strict mode"
    );
    Ok(())
}

async fn run_repair(client: &Client, command: RepairCommand, json_output: bool) -> Result<()> {
    match command {
        RepairCommand::DryRun => {
            let plan: OperationalRepairPlan = client.get("/v1/repair").await?;
            if json_output {
                print_value(&plan, true)?;
            } else {
                if plan.items.is_empty() {
                    println!("No operational repairs are needed.");
                    return Ok(());
                }
                println!(
                    "REPAIR PLAN  {} items · snapshot {}",
                    plan.items.len(),
                    plan.snapshot_index
                );
                for item in &plan.items {
                    println!(
                        "{}\t{}\t{}\t{}",
                        item.class, item.subject, item.reason, item.id
                    );
                    for subject in &item.affected_subjects {
                        println!("  affected\t{subject}");
                    }
                }
                println!("Apply this exact plan with: st repair apply {}", plan.token);
            }
        }
        RepairCommand::Apply { token } => {
            let result: OperationalRepairResult = client
                .post("/v1/repair/apply", &OperationalRepairApplyRequest { token })
                .await?;
            if json_output {
                print_value(&result, true)?;
            } else {
                println!(
                    "repair\tapplied={}\talready_applied={}\t{}",
                    result.applied, result.already_applied, result.token
                );
                for subject in &result.affected_subjects {
                    println!("  affected\t{subject}");
                }
                if let Some(receipt) = &result.receipt_claim_id {
                    println!("  receipt\t{receipt}");
                }
            }
        }
    }
    Ok(())
}

/// Each peer's line, then how far apart the two envelope sets are and how long catching up
/// should take, in words.
fn render_replication_peers(peers: &[ReplicationPeerStatus], now: u128) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    for peer in peers
        .iter()
        .filter(|peer| peer.sync.as_ref().is_some_and(|sync| sync.catching_up))
    {
        let sync = peer.sync.as_ref().expect("filtered on sync");
        let _ = writeln!(
            output,
            "sync\tcatching up: {} has {} this node lacks, {}",
            peer.peer,
            envelope_count(sync.peer_only_envelopes),
            catch_up_estimate(sync.estimated_catch_up_seconds)
        );
    }
    for peer in peers {
        let _ = writeln!(
            output,
            "peer\t{}\t{}\t{}",
            peer.peer,
            peer.status,
            peer.last_error.as_deref().unwrap_or("")
        );
        if let Some(at) = peer.last_success_at_unix_ms {
            let _ = writeln!(output, "  last exchange {}", relative_time(at, now));
        } else {
            let _ = writeln!(output, "  no exchange yet");
        }
        let Some(sync) = &peer.sync else {
            let _ = writeln!(output, "  difference not measured yet");
            continue;
        };
        if sync.peer_only_envelopes == 0 && sync.local_only_envelopes == 0 {
            let _ = writeln!(
                output,
                "  in sync: neither side has an envelope the other lacks (measured {})",
                relative_time(sync.measured_at_unix_ms, now)
            );
            continue;
        }
        let _ = writeln!(
            output,
            "  {} has {} this node lacks",
            peer.peer,
            envelope_count(sync.peer_only_envelopes)
        );
        let _ = writeln!(
            output,
            "  this node has {} {} lacks",
            envelope_count(sync.local_only_envelopes),
            peer.peer
        );
        if sync.peer_only_envelopes != 0 {
            let rate = sync
                .receive_rate_per_second
                .map(|rate| format!("receiving {rate:.1} envelopes/s, "))
                .unwrap_or_default();
            let _ = writeln!(
                output,
                "  {rate}{} (measured {})",
                catch_up_estimate(sync.estimated_catch_up_seconds),
                relative_time(sync.measured_at_unix_ms, now)
            );
        }
    }
    output
}

async fn run_replication(
    client: &Client,
    command: ReplicationCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        ReplicationCommand::Status => {
            let status: ReplicationStatus = client.get("/v1/replication/status").await?;
            if json_output {
                return print_value(&status, true);
            }
            println!(
                "fleet\t{}",
                status.fleet_id.as_deref().unwrap_or("local-only")
            );
            println!("authority-digest\t{}", status.authority_digest);
            println!("graph-digest\t{}", status.graph_digest);
            println!("envelopes\t{}", status.received_envelopes);
            println!(
                "records\tvalid={} pending={} unknown={} invalid={} repaired={}",
                status.valid_records,
                status.pending_records,
                status.unknown_records,
                status.invalid_records,
                status.repaired_records
            );
            println!("unhealthy-projections\t{}", status.unhealthy_projections);
            print!("{}", render_replication_peers(&status.peers, now_ms()));
            Ok(())
        }
        ReplicationCommand::Invalid { all } => {
            let records: Vec<ReplicaRecordView> = client
                .get(&format!(
                    "/v1/replication/records?unresolved={}",
                    if all { "false" } else { "true" }
                ))
                .await?;
            if json_output {
                return print_value(&records, true);
            }
            if records.is_empty() {
                println!(
                    "No {} replication records.",
                    if all { "stored" } else { "unresolved" }
                );
                return Ok(());
            }
            for record in records {
                println!(
                    "{}\t{}\t{}:{}\t{}\t{}",
                    record.state,
                    record.record_ref,
                    record.writer,
                    record.sequence,
                    record.subject.as_deref().unwrap_or("unknown-subject"),
                    record.error_message.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        ReplicationCommand::Inspect { record } => {
            let record: ReplicaRecordView = client
                .get(&format!(
                    "/v1/replication/records/{}",
                    urlencoding::encode(record.strip_prefix("record/").unwrap_or(&record))
                ))
                .await?;
            if json_output {
                return print_value(&record, true);
            }
            println!("record\t{}", record.record_ref);
            println!("state\t{}", record.state);
            println!("writer\t{}", record.writer);
            println!("sequence\t{}", record.sequence);
            println!("envelope\t{}", record.envelope_hash);
            println!("position\t{}", record.position);
            println!("claim\t{}", record.claim_id.as_deref().unwrap_or(""));
            println!("subject\t{}", record.subject.as_deref().unwrap_or(""));
            println!("kind\t{}", record.kind.as_deref().unwrap_or(""));
            println!("error-code\t{}", record.error_code.as_deref().unwrap_or(""));
            println!("error\t{}", record.error_message.as_deref().unwrap_or(""));
            println!(
                "replacement\t{}",
                record.replacement_claim_id.as_deref().unwrap_or("")
            );
            Ok(())
        }
        ReplicationCommand::Diff { peer } => {
            let status: ReplicationStatus = client.get("/v1/replication/status").await?;
            let remote = status
                .peers
                .iter()
                .find(|item| item.peer == peer)
                .with_context(|| format!("peer `{peer}` is not configured"))?;
            let value = json!({
                "peer": peer,
                "status": remote.status,
                "authority": {
                    "local": status.authority_digest,
                    "remote": remote.authority_digest,
                    "equal": remote.authority_digest.as_deref() == Some(status.authority_digest.as_str()),
                },
                "graph": {
                    "local": status.graph_digest,
                    "remote": remote.graph_digest,
                    "equal": remote.graph_digest.as_deref() == Some(status.graph_digest.as_str()),
                },
            });
            if json_output {
                return print_value(&value, true);
            }
            println!("peer\t{}\t{}", peer, remote.status);
            println!(
                "authority\t{}\t{}\t{}",
                if remote.authority_digest.as_deref() == Some(status.authority_digest.as_str()) {
                    "equal"
                } else {
                    "different"
                },
                status.authority_digest,
                remote.authority_digest.as_deref().unwrap_or("unknown")
            );
            println!(
                "graph\t{}\t{}\t{}",
                if remote.graph_digest.as_deref() == Some(status.graph_digest.as_str()) {
                    "equal"
                } else {
                    "different"
                },
                status.graph_digest,
                remote.graph_digest.as_deref().unwrap_or("unknown")
            );
            Ok(())
        }
        ReplicationCommand::Repair {
            record,
            replacement_claim,
            reason,
            actor,
            idempotency_key,
        } => {
            let record_ref = if record.starts_with("record/") {
                record
            } else {
                format!("record/{record}")
            };
            let idempotency_key = idempotency_key.unwrap_or_else(|| {
                hex::encode(Sha256::digest(
                    format!("repair\0{record_ref}\0{replacement_claim}\0{reason}\0{actor}")
                        .as_bytes(),
                ))
            });
            let request = ReplicationRepairRequest {
                record_ref,
                replacement_claim_id: replacement_claim,
                reason,
                actor,
                idempotency_key,
            };
            let claim: ClaimRecord = client.post("/v1/replication/repair", &request).await?;
            if json_output {
                print_value(&claim, true)
            } else {
                println!("repaired\t{}", claim.id);
                Ok(())
            }
        }
    }
}

fn run_service(command: ServiceCommand, json_output: bool) -> Result<()> {
    match command {
        ServiceCommand::Install { config } => {
            st3::service::install(Config::load_with_fleet(config.as_deref())?)
        }
        ServiceCommand::Status => {
            let report = st3::service::status()?;
            if json_output {
                print_value(&report, true)
            } else {
                println!("SERVICES  {}", report.manager);
                for service in report.services {
                    println!(
                        "{}  {}  {}",
                        service.name,
                        if service.installed {
                            "installed"
                        } else {
                            "not-installed"
                        },
                        service.state
                    );
                }
                Ok(())
            }
        }
        ServiceCommand::Permissions { open } => {
            if json_output {
                let guidance = st3::service::permissions_guidance()?;
                if open {
                    st3::service::open_permissions_settings()?;
                }
                print_value(
                    &serde_json::json!({
                        "platform": std::env::consts::OS,
                        "guidance": guidance,
                        "settings_opened": open && cfg!(target_os = "macos"),
                    }),
                    true,
                )
            } else {
                st3::service::permissions(open)
            }
        }
        ServiceCommand::Restart { config } => {
            st3::service::restart(Config::load_with_fleet(config.as_deref())?)
        }
        ServiceCommand::Reset { config } => {
            let config = Config::load(config.as_deref())?;
            confirm_service_reset(&config)?;
            st3::service::reset(config)
        }
        ServiceCommand::Uninstall => st3::service::uninstall(),
    }
}

fn confirm_service_reset(config: &Config) -> Result<()> {
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "st service reset requires an interactive terminal"
    );
    let mut answer = String::new();
    for (prompt, expected) in [
        ("Erase all st state? Type `yes`: ", "yes"),
        (
            &format!("Type the node name `{}`: ", config.node),
            config.node.as_str(),
        ),
        ("Type `erase st state`: ", "erase st state"),
    ] {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        answer.clear();
        std::io::stdin().read_line(&mut answer)?;
        anyhow::ensure!(
            answer.trim() == expected,
            "the st state reset was cancelled"
        );
    }
    Ok(())
}

async fn status_for(client: &Client, subject: &str) -> Result<StatusResponse> {
    client
        .get(&format!(
            "/v1/status?subject={}",
            urlencoding::encode(subject)
        ))
        .await
}

async fn session_incarnation(client: &Client, subject: &str) -> Result<String> {
    let status = status_for(client, subject).await?;
    status
        .subjects
        .first()
        .and_then(|item| item.actual.as_ref())
        .map(|actual| actual.get("fields").unwrap_or(actual))
        .and_then(|fields| fields.get("incarnation_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("subject `{subject}` has no live incarnation"))
}

fn normalize_member_subject(subject: &str, namespace: &str) -> String {
    if subject.contains('/') {
        subject.into()
    } else {
        format!("{namespace}/{subject}")
    }
}

async fn run_doc(client: &Client, command: DocCommand, json_output: bool) -> Result<()> {
    match command {
        DocCommand::Put { file, name } => {
            let metadata = fs::symlink_metadata(&file)
                .with_context(|| format!("inspect document {}", file.display()))?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "document input cannot be a symbolic link"
            );
            anyhow::ensure!(metadata.is_file(), "document input must be a regular file");
            let bytes =
                fs::read(&file).with_context(|| format!("read document {}", file.display()))?;
            let local_hash = hex::encode(Sha256::digest(&bytes));
            let versions: DocumentListResponse = client
                .get(&format!(
                    "/v1/documents?name={}",
                    urlencoding::encode(&name)
                ))
                .await?;
            let selected = versions.items.iter().find(|version| version.latest);
            if let Some(selected) = selected.filter(|version| version.hash != local_hash) {
                eprintln!(
                    "warning: local bytes have hash {local_hash}; the selected binding has hash {}",
                    selected.hash
                );
            }
            let response: DocumentVersion = client
                .post(
                    "/v1/documents",
                    &DocumentPutRequest {
                        idempotency_key: format!("document:{name}:{local_hash}"),
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
        DocCommand::Get { reference, output } => {
            let reference = if reference.contains('@') {
                reference
            } else {
                let versions: DocumentListResponse = client
                    .get(&format!(
                        "/v1/documents?name={}&limit=1",
                        urlencoding::encode(&reference)
                    ))
                    .await?;
                let selected = versions
                    .items
                    .into_iter()
                    .find(|version| version.latest)
                    .with_context(|| format!("document `{reference}` does not exist"))?;
                format!("{}@{}", selected.name, selected.hash)
            };
            let path = format!(
                "/v1/documents/content?reference={}",
                urlencoding::encode(&reference)
            );
            let response: Value = client.get(&path).await?;
            let bytes = serde_json::from_value::<Vec<u8>>(
                response
                    .get("bytes")
                    .cloned()
                    .context("document response lacks bytes")?,
            )?;
            if let Some(output) = output {
                fs::write(&output, bytes)
                    .with_context(|| format!("write document {}", output.display()))?;
            } else {
                use std::io::Write as _;
                std::io::stdout().write_all(&bytes)?;
            }
            Ok(())
        }
        DocCommand::Ls {
            name,
            all,
            limit,
            cursor,
        } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the document limit must be 1 through 200"
            );
            let mut path = name.as_deref().map_or_else(
                || "/v1/documents?".to_owned(),
                |name| format!("/v1/documents?prefix={}&", urlencoding::encode(name)),
            );
            path.push_str(&format!("history={all}&limit={limit}"));
            if let Some(cursor) = &cursor {
                path.push_str(&format!("&cursor={}", urlencoding::encode(cursor)));
            }
            let response: DocumentListResponse = client.get(&path).await?;
            if json_output {
                print_value(&response, true)
            } else {
                if response.items.is_empty() {
                    println!("No documents.");
                }
                for version in response.items {
                    let latest = if version.latest { " latest" } else { "" };
                    let hash = if all {
                        format!("@{}", version.hash)
                    } else {
                        String::new()
                    };
                    let created = chrono::DateTime::from_timestamp_millis(
                        version.created_at_unix_ms.min(i64::MAX as u128) as i64,
                    )
                    .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                    .unwrap_or_else(|| "unknown-date".into());
                    println!(
                        "{}{} {} bytes · {} · {}{latest}",
                        version.name,
                        hash,
                        version.size,
                        created,
                        version.owner.as_deref().unwrap_or("unknown-owner")
                    );
                }
                if response.has_more
                    && let Some(cursor) = response.next_cursor
                {
                    println!(
                        "More document versions are available. Continue with: {}",
                        document_continuation_command(name.as_deref(), all, limit, &cursor)
                    );
                }
                Ok(())
            }
        }
    }
}

fn document_continuation_command(
    name: Option<&str>,
    all: bool,
    limit: usize,
    cursor: &str,
) -> String {
    let name = name
        .map(|name| format!(" {}", shell_argument(name)))
        .unwrap_or_default();
    format!(
        "st documents ls{name} --limit {limit}{} --cursor {}",
        if all { " --all" } else { "" },
        shell_argument(cursor)
    )
}

async fn run_import(endpoint: &Endpoint, command: ImportCommand, json_output: bool) -> Result<()> {
    match command {
        ImportCommand::Ls { all, cursor, limit } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the import limit must be 1 through 200"
            );
            let client = generated_client(endpoint, None)?;
            let response = client
                .sessions_list_native(cursor.as_deref(), Some(limit), all)
                .await?;
            if json_output {
                return print_value(&response, true);
            }
            println!("NATIVE SESSIONS  {}", response.value.items.len());
            if response.value.items.is_empty() {
                println!("No native harness sessions found.");
            }
            let next_cursor = response.value.page.next_cursor.clone();
            for resource in response.value.items {
                if let ClientResource::Session(session) = resource {
                    print!("{}", render_import_session(&session));
                }
            }
            if let Some(cursor) = next_cursor {
                let history = if all { " --all" } else { "" };
                println!(
                    "More sessions are available: st import ls{history} --cursor {cursor} --limit {limit}"
                );
            }
            Ok(())
        }
        ImportCommand::Show { session } => {
            let response = generated_client(endpoint, None)?
                .sessions_get(&session)
                .await?;
            if json_output {
                return print_value(&response, true);
            }
            let ClientResource::Session(session) = response.value else {
                anyhow::bail!("`{session}` is not a session resource");
            };
            anyhow::ensure!(
                session.extra.get("managed") == Some(&Value::Bool(false)),
                "`{}` is already managed by st",
                session.header.id
            );
            print!("{}", render_import_session(&session));
            Ok(())
        }
        ImportCommand::Run { session, person } => {
            let client = generated_client(endpoint, Some(&person))?;
            let resource = client.sessions_get(&session).await?;
            let ClientResource::Session(native) = &resource.value else {
                anyhow::bail!("`{session}` is not a session resource");
            };
            anyhow::ensure!(
                native.extra.get("managed") == Some(&Value::Bool(false)),
                "`{}` is already managed by st",
                native.header.id
            );
            anyhow::ensure!(
                native.extra.get("importable") == Some(&Value::Bool(true)),
                "{}",
                native
                    .extra
                    .get("import_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("the native session is not importable")
            );
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let result = client
                .session_import(
                    format!("action/{nonce}"),
                    format!("session-import:{nonce}"),
                    ClientFence {
                        snapshot_id: resource.snapshot.id,
                        subject_revisions: BTreeMap::from([(
                            native.header.id.clone(),
                            native.header.revision.clone(),
                        )]),
                        ..ClientFence::default()
                    },
                    ClientTargetParameters {
                        target_id: native.header.id.clone(),
                        ..ClientTargetParameters::default()
                    },
                )
                .await?;
            print_client_value(&result, json_output)
        }
    }
}

fn render_import_session(session: &st3_client::Session) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    let driver = session
        .extra
        .get("driver")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let native = session
        .extra
        .get("native_session_id")
        .and_then(Value::as_str);
    let workspace = session
        .extra
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or("unknown workspace");
    let importable = session
        .extra
        .get("importable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let _ = writeln!(
        output,
        "{}  {}  {}  {}",
        session.header.id, driver, session.state, workspace
    );
    if let Some(native) = native {
        let _ = writeln!(output, "  native session: {native}");
    }
    if let Some(process) = session.extra.get("process").and_then(Value::as_object) {
        let _ = writeln!(
            output,
            "  process: pid {} · exact {}",
            process
                .get("pid")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            process
                .get("exact_session")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
    }
    if importable {
        let _ = writeln!(
            output,
            "  import: st import run {} --as person/NAME",
            session.header.id
        );
        let _ = writeln!(
            output,
            "  conversation: st conversations timeline {}",
            session.header.id
        );
    } else if let Some(reason) = session.extra.get("import_reason").and_then(Value::as_str) {
        let _ = writeln!(output, "  blocked: {reason}");
    }
    output
}

async fn run_agents(
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    command: AgentsCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        AgentsCommand::Queue(args) => {
            run_agent_queue(endpoint, configured_person, args, json_output).await
        }
        AgentsCommand::Apply(args) => {
            let client = cli_client(endpoint);
            let (kdl, source_name) = read_intent(Some(&args.file))?;
            let response = publish_text(
                &client,
                kdl,
                source_name.unwrap_or_else(|| "standard input".into()),
                args.actor,
            )
            .await?;
            print_value(&response, json_output)
        }
        AgentsCommand::Start(args) => {
            let kdl = agent_start_document(&args)?;
            if args.print_kdl {
                print!("{kdl}");
                return Ok(());
            }
            let response = publish_text(
                &cli_client(endpoint),
                kdl,
                format!("st agents start {}", args.identity),
                args.actor,
            )
            .await?;
            print_value(&response, json_output)
        }
        AgentsCommand::Stop(args) => {
            let subject = normalize_member_subject(&args.subject, "agent");
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

fn agent_start_document(args: &AgentStartArgs) -> Result<String> {
    let mut agent = KdlNode::new("agent");
    agent
        .entries_mut()
        .push(KdlEntry::new(args.identity.clone()));
    let mut body = KdlDocument::new();
    if let Some(host) = &args.host {
        body.nodes_mut().push(kdl_node("host", [host.as_str()]));
    }
    let workspace = fs::canonicalize(&args.workspace)
        .with_context(|| format!("resolve workspace {}", args.workspace.display()))?
        .display()
        .to_string();
    body.nodes_mut()
        .push(kdl_node("workspace", [workspace.as_str()]));
    body.nodes_mut().push(kdl_node("restart", ["always"]));

    let mut harness = KdlNode::new("harness");
    harness
        .entries_mut()
        .push(KdlEntry::new(args.harness.clone()));
    let mut harness_body = KdlDocument::new();
    if let Some(model) = &args.model {
        harness_body
            .nodes_mut()
            .push(kdl_node("model", [model.as_str()]));
    }
    if let Some(effort) = &args.effort {
        harness_body
            .nodes_mut()
            .push(kdl_node("effort", [effort.as_str()]));
    }
    if let Some(prompt) = &args.prompt {
        harness_body
            .nodes_mut()
            .push(kdl_node("prompt", [prompt.as_str()]));
    }
    if !args.arguments.is_empty() {
        let mut arguments = KdlNode::new("args");
        arguments
            .entries_mut()
            .extend(args.arguments.iter().cloned().map(KdlEntry::new));
        harness_body.nodes_mut().push(arguments);
    }
    harness.set_children(harness_body);
    body.nodes_mut().push(harness);
    agent.set_children(body);
    Ok(publication_document(agent))
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
                .agents_get(&subject)
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
        AgentsCommand::Apply(_)
        | AgentsCommand::Start(_)
        | AgentsCommand::Stop(_)
        | AgentsCommand::Queue(_) => {
            unreachable!("agent mutation and queue commands return before inspection")
        }
    };
    anyhow::ensure!(
        args.limit > 0 && args.limit <= 200,
        "the agent limit must be 1 through 200"
    );
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
        return print_value(&response, true);
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
    Ok(())
}

fn seat_subject(value: &str) -> String {
    if value.starts_with("agent/") {
        value.to_owned()
    } else {
        format!("agent/{value}")
    }
}

fn mission_run_subject(value: &str) -> String {
    if value.starts_with("mission-run/") {
        value.to_owned()
    } else {
        format!("mission-run/{value}")
    }
}

/// Shared by `st agents queue AGENT` and `st missions queued AGENT`.
async fn show_agent_queue(endpoint: &Endpoint, agent: &str, json_output: bool) -> Result<()> {
    let agent = seat_subject(agent);
    let response = generated_client(endpoint, None)?
        .agent_queue(&agent)
        .await?;
    if json_output {
        return print_value(&response, true);
    }
    print!("{}", render_agent_queue(&response.value));
    Ok(())
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

fn render_missions_tree(response: &Value) -> String {
    use std::fmt::Write as _;
    let value = &response["value"];
    let mut output = String::from("RUNNING MISSIONS\n");
    let runs = value["runs"].as_array();
    if runs.is_none_or(Vec::is_empty) {
        output.push_str("  none\n");
    }
    for run in runs.into_iter().flatten() {
        let steps = run["steps"].as_array();
        let done = steps
            .into_iter()
            .flatten()
            .filter(|step| step["state"] == "completed")
            .count();
        let active_step = steps
            .into_iter()
            .flatten()
            .find(|step| step["state"] == "claimed")
            .or_else(|| {
                steps
                    .into_iter()
                    .flatten()
                    .find(|step| step["state"] == "ready")
            })
            .and_then(|step| step["id"].as_str());
        let active = steps
            .into_iter()
            .flatten()
            .find(|step| step["id"].as_str() == active_step)
            .and_then(|step| step["name"].as_str())
            .unwrap_or("waiting");
        let pending = steps
            .into_iter()
            .flatten()
            .filter(|step| step["state"] != "completed" && step["id"].as_str() != active_step)
            .filter_map(|step| step["name"].as_str())
            .collect::<Vec<_>>();
        let mission = run["mission"].as_str().unwrap_or("unknown");
        let _ = writeln!(
            output,
            "  {mission}: {active} → {}  ({done} done)",
            if pending.is_empty() {
                "done".to_owned()
            } else {
                pending.join(" → ")
            }
        );
    }
    output.push_str("STANDING QUEUES\n");
    let queues = value["standing_queues"].as_array();
    if queues.is_none_or(Vec::is_empty) {
        output.push_str("  none\n");
    }
    for queue in queues.into_iter().flatten() {
        let id = queue["agent_id"].as_str().unwrap_or("unknown");
        let current = queue["current_work_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let next = queue["next_work_id"].as_str().unwrap_or("none");
        let waiting = queue["runs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|run| run["state"] == "waiting")
            .filter_map(|run| run["mission_run_id"].as_str())
            .collect::<Vec<_>>();
        let _ = writeln!(
            output,
            "  {id}: current {} · next {next} · waiting {}",
            if current.is_empty() {
                "none".to_owned()
            } else {
                current.join(", ")
            },
            if waiting.is_empty() {
                "none".to_owned()
            } else {
                waiting.join(", ")
            }
        );
    }
    output.push_str("UNSTARTED MISSIONS\n");
    let unstarted = value["unstarted_missions"].as_array();
    if unstarted.is_none_or(Vec::is_empty) {
        output.push_str("  none\n");
    }
    for mission in unstarted.into_iter().flatten() {
        let suffix = if mission["state"] == "draft" {
            " (draft)"
        } else {
            ""
        };
        let _ = writeln!(
            output,
            "  {}{suffix}",
            mission["title"].as_str().unwrap_or("unknown")
        );
    }
    output.push_str("AGENTS BY HOST\n");
    let mut grouped = BTreeMap::<String, BTreeMap<String, Vec<&Value>>>::new();
    for agent in value["agents"].as_array().into_iter().flatten() {
        let host = agent["host_id"].as_str().unwrap_or("unknown").to_owned();
        let kind = agent["seat_kind"].as_str().unwrap_or("standing").to_owned();
        grouped
            .entry(host)
            .or_default()
            .entry(kind)
            .or_default()
            .push(agent);
    }
    if grouped.is_empty() {
        output.push_str("  none\n");
    }
    for (host, kinds) in grouped {
        let _ = writeln!(output, "  {host}");
        for kind in ["standing", "mission"] {
            let Some(agents) = kinds.get(kind) else {
                continue;
            };
            let _ = writeln!(output, "    {kind}");
            for agent in agents {
                let state = if agent["harness_state"] == "working" {
                    "working"
                } else if agent["state"] == "running" {
                    "idle"
                } else {
                    "waiting"
                };
                let _ = writeln!(
                    output,
                    "      {}  {} / {} / {}  {state}",
                    agent["name"].as_str().unwrap_or("unknown"),
                    agent["driver"].as_str().unwrap_or("unknown"),
                    agent["model"].as_str().unwrap_or("default"),
                    agent["effort"].as_str().unwrap_or("default")
                );
            }
        }
    }
    output
}

fn render_agent_queue(queue: &st3_client::AgentQueue) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "AGENT QUEUE  {}", queue.agent_id);
    if queue.current_work_ids.is_empty() {
        let _ = writeln!(output, "CURRENT      none");
    }
    for current in &queue.current_work_ids {
        let _ = writeln!(output, "CURRENT      {current}");
    }
    let _ = writeln!(
        output,
        "NEXT WORK    {}",
        queue.next_work_id.as_deref().unwrap_or("none")
    );
    let _ = writeln!(output, "RUNS         {}", queue.runs.len());
    if queue.runs.is_empty() {
        let _ = writeln!(output, "  No mission runs are queued for this seat.");
    }
    for run in &queue.runs {
        let detail = match run.state.as_str() {
            "claimed" => run.claimed_work_ids.join(", "),
            "ready" => {
                let first = run.ready_work_ids.first().map_or("", String::as_str);
                let marker = if queue.next_work_id.as_deref() == Some(first) {
                    "next "
                } else {
                    ""
                };
                match run.ready_work_ids.len() {
                    0 | 1 => format!("{marker}{first}"),
                    count => format!("{marker}{first} (+{} ready)", count - 1),
                }
            }
            _ if run.waiting_work_ids.is_empty() => "no open step for this seat".into(),
            _ => format!("{} not ready", run.waiting_work_ids.join(", ")),
        };
        let detail = match &run.waiting_for_run_id {
            Some(after) if run.state == "waiting" => {
                format!("{detail}; waiting for {after} to complete")
            }
            _ => detail,
        };
        let run_state = if run.run_state == "running" {
            String::new()
        } else {
            format!(" (run {})", run.run_state)
        };
        let _ = writeln!(
            output,
            "  {}. {}  {}{}  {}",
            run.position, run.mission_run_id, run.state, run_state, detail
        );
    }
    let _ = writeln!(output, "MOVES        {} total", queue.move_count);
    for moved in &queue.moves {
        let placement = match (moved.placement, moved.anchor_run_id.as_deref()) {
            (st3_client::AgentQueuePlacement::Top, _) => "to the top".to_owned(),
            (st3_client::AgentQueuePlacement::Bottom, _) => "to the bottom".to_owned(),
            (st3_client::AgentQueuePlacement::Before, anchor) => {
                format!("before {}", anchor.unwrap_or("another run"))
            }
            (st3_client::AgentQueuePlacement::After, anchor) => {
                format!("after {}", anchor.unwrap_or("another run"))
            }
        };
        let _ = write!(
            output,
            "  {}  {} moved {} {placement}",
            moved.moved_at,
            moved.actor_id.as_deref().unwrap_or("unknown"),
            moved.mission_run_id
        );
        if let Some(reason) = moved.reason.as_deref() {
            let _ = write!(output, ": {reason}");
        }
        let _ = writeln!(output);
    }
    output
}

fn render_client_agent(
    agent: &st3_client::Agent,
    current_steps: &[StepRunView],
    now_unix_ms: u128,
) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "AGENT  {}", agent.header.id);
    let _ = writeln!(output, "NAME         {}", agent.name);
    let _ = writeln!(output, "STATE        {}", agent.state);
    let _ = writeln!(output, "REACHABILITY {}", agent.reachability);
    let _ = writeln!(
        output,
        "HARNESS      {} · {}",
        agent.driver.as_deref().unwrap_or("none"),
        agent.harness_state.as_deref().unwrap_or("unobserved")
    );
    if let Some(fault) = &agent.fault {
        let _ = writeln!(output, "FAULT        {fault}");
    }
    if let Some(incarnation) = &agent.incarnation_id {
        let _ = writeln!(output, "INCARNATION  {incarnation}");
    }
    if let Some(owner) = &agent.owner_run_id {
        let _ = writeln!(output, "MISSION      {owner}");
    }
    for current in &agent.current_work_ids {
        let _ = writeln!(output, "CURRENT WORK {current}");
        let Some(step) = current_steps.iter().find(|step| step.subject == *current) else {
            continue;
        };
        let _ = writeln!(
            output,
            "CURRENT STEP {} · {}",
            step.title.as_deref().unwrap_or(&step.step),
            step.status
        );
        // A submitted step awaiting verification is still held by its worker.
        if let Some(summary) = &step.completion_summary {
            let _ = writeln!(output, "DONE         {}", glance(summary));
        } else if let (Some(summary), Some(at)) = (&step.progress_summary, step.progress_at_unix_ms)
        {
            let _ = writeln!(
                output,
                "PROGRESS     {} · {}",
                glance(summary),
                relative_time(at, now_unix_ms)
            );
        } else {
            let _ = writeln!(output, "PROGRESS     none reported");
        }
    }
    if agent.active_work_count > agent.current_work_ids.len() as u64 {
        let _ = writeln!(output, "ACTIVE WORK  {} total", agent.active_work_count);
    }
    if let Some(next) = &agent.next_work_id {
        let _ = writeln!(output, "NEXT WORK    {next}");
        let _ = writeln!(output, "QUEUED WORK  {} total", agent.queued_work_count);
        for upcoming in agent.upcoming_work_ids.iter().skip(1) {
            let _ = writeln!(output, "UPCOMING     {upcoming}");
        }
    }
    for runtime in &agent.runtime_ids {
        let _ = writeln!(output, "RUNTIME      {runtime}");
    }
    output
}

fn render_client_agents(
    page: &ClientPage,
    tree: bool,
    enrich: bool,
    continuation_command: &str,
) -> String {
    use std::fmt::Write as _;

    let agents = page
        .items
        .iter()
        .filter_map(|item| match item {
            ClientResource::Agent(agent) => Some(agent),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut output = String::new();
    let _ = writeln!(
        output,
        "{}  {}",
        if tree { "AGENT TREE" } else { "AGENTS" },
        agents.len()
    );
    if agents.is_empty() {
        let _ = writeln!(output, "No operational agents.");
        return output;
    }
    if !tree {
        for agent in &agents {
            if enrich {
                let _ = writeln!(
                    output,
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    agent.header.id,
                    agent.name,
                    agent.state,
                    agent.reachability,
                    agent.driver.as_deref().unwrap_or("-"),
                    agent.harness_state.as_deref().unwrap_or("-"),
                    agent.owner_run_id.as_deref().unwrap_or("-"),
                );
                let _ = writeln!(
                    output,
                    "  incarnation {}",
                    agent.incarnation_id.as_deref().unwrap_or("-")
                );
                if let Some(current) = agent.current_work_ids.first() {
                    let _ = writeln!(output, "  current {current}");
                }
                if let Some(next) = &agent.next_work_id {
                    let _ = writeln!(output, "  next {next} ({} queued)", agent.queued_work_count);
                }
            } else {
                let _ = writeln!(
                    output,
                    "{}\t{}\t{}",
                    agent.header.id, agent.name, agent.state
                );
            }
            for relationship in &agent.under {
                match relationship.reason.as_deref() {
                    Some(reason) => {
                        let _ = writeln!(output, "  under {} ({reason})", relationship.agent_id);
                    }
                    None => {
                        let _ = writeln!(output, "  under {}", relationship.agent_id);
                    }
                }
            }
        }
    } else {
        let mut groups = BTreeMap::<String, Vec<&st3_client::Agent>>::new();
        for agent in agents {
            let owner = agent
                .owner_run_id
                .as_deref()
                .unwrap_or("unowned")
                .strip_prefix("mission-run/")
                .unwrap_or_else(|| agent.owner_run_id.as_deref().unwrap_or("unowned"));
            groups.entry(owner.to_owned()).or_default().push(agent);
        }
        let group_count = groups.len();
        for (group_index, (owner, mut members)) in groups.into_iter().enumerate() {
            members.sort_by(|left, right| left.header.id.cmp(&right.header.id));
            let group_branch = if group_index + 1 == group_count {
                "└─"
            } else {
                "├─"
            };
            let _ = writeln!(output, "{group_branch} {owner}");
            let member_prefix = if group_index + 1 == group_count {
                "   "
            } else {
                "│  "
            };
            for (member_index, agent) in members.iter().enumerate() {
                let branch = if member_index + 1 == members.len() {
                    "└─"
                } else {
                    "├─"
                };
                let state = agent.harness_state.as_deref().unwrap_or(&agent.state);
                let name = agent
                    .header
                    .id
                    .rsplit('/')
                    .next()
                    .unwrap_or(&agent.header.id);
                let _ = writeln!(
                    output,
                    "{member_prefix}{branch} {name}  {state} · {}",
                    agent.reachability
                );
                let _ = writeln!(output, "{member_prefix}   {}", agent.header.id);
            }
        }
    }
    if let Some(cursor) = page.page.next_cursor.as_deref() {
        let _ = writeln!(
            output,
            "More agents are available: {continuation_command} --cursor {cursor} --limit {}",
            page.page.limit
        );
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

/// A harness process names its own seat in `ST_AGENT`. The local API trusts the actor a command
/// names, so a model that inferred the wrong identity could otherwise read and send as a peer seat.
/// The process may still act as a non-agent subject, such as its exec or a person its work names.
fn reject_foreign_agent_actor(actor: &str) -> Result<()> {
    let own = std::env::var("ST_AGENT").ok();
    let mission_run = std::env::var("ST_MISSION_RUN")
        .ok()
        .filter(|value| !value.is_empty());
    match foreign_agent_actor(actor, own.as_deref(), mission_run.as_deref()) {
        Some(message) => anyhow::bail!(message),
        None => Ok(()),
    }
}

fn foreign_agent_actor(
    actor: &str,
    own: Option<&str>,
    mission_run: Option<&str>,
) -> Option<String> {
    let own = own.map(str::trim).filter(|own| own.starts_with("agent/"))?;
    let actor = actor.trim();
    let actor = if actor.starts_with("agent/") {
        actor.to_owned()
    } else if actor.contains('/') {
        return None;
    } else {
        normalize_message_subject_in_run(actor, mission_run)
    };
    (actor.starts_with("agent/") && actor != own).then(|| {
        format!(
            "this harness is `{own}` (ST_AGENT) and cannot act as `{actor}`; use `--as \"$ST_AGENT\"` or `--from \"$ST_AGENT\"`"
        )
    })
}

fn normalize_message_subject(value: &str) -> String {
    let mission_run = std::env::var("ST_MISSION_RUN")
        .ok()
        .filter(|value| !value.is_empty());
    normalize_message_subject_in_run(value, mission_run.as_deref())
}

fn normalize_message_subject_in_run(value: &str, mission_run: Option<&str>) -> String {
    if value == "requester" {
        "person/requester".into()
    } else if value.contains('/') {
        value.into()
    } else if let Some(mission_run) = mission_run {
        format!("agent/{mission_run}/{value}")
    } else {
        format!("agent/{value}")
    }
}

async fn document_bytes(client: &Client, name: &str, hash: &str) -> Result<Vec<u8>> {
    let value: Value = client
        .get(&format!(
            "/v1/documents/content?reference={}",
            urlencoding::encode(&format!("{name}@{hash}"))
        ))
        .await?;
    serde_json::from_value(
        value
            .get("bytes")
            .cloned()
            .context("document response lacks bytes")?,
    )
    .map_err(Into::into)
}

fn normalize_agent_subject(identity: &str) -> String {
    if identity.starts_with("agent/") {
        identity.to_owned()
    } else {
        format!("agent/{identity}")
    }
}

async fn run_claim(client: &Client, args: ClaimArgs, json_output: bool) -> Result<()> {
    if let Some(actor) = &args.actor {
        reject_foreign_agent_actor(actor)?;
    }
    let response: ClaimRecord = client
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: args.subject,
                kind: args.kind,
                actor: args.actor,
                fields: args.fields.into_iter().collect(),
                evidence: args.evidence,
                expected_subject: None,
                idempotency_key: args.idempotency_key,
            },
        )
        .await?;
    if json_output {
        print_value(&response, true)
    } else {
        println!("{}", response.id);
        Ok(())
    }
}

async fn run_harness_diagnostic(
    client: &Client,
    args: HarnessDiagnosticArgs,
    json_output: bool,
) -> Result<()> {
    let actor = normalize_agent_subject(&args.actor);
    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&(
        actor.as_str(),
        args.incarnation.as_deref(),
        args.code.as_str(),
        args.reason.as_str(),
        args.severity.as_str(),
        args.status.as_str(),
    ))?));
    let response: ClaimRecord = client
        .post(
            "/v1/diagnostics/harness",
            &json!({
                "actor": actor,
                "code": args.code,
                "reason": args.reason,
                "severity": args.severity,
                "status": args.status,
                "incarnation_id": args.incarnation,
                "idempotency_key": format!("harness-diagnostic:{}", &digest[..32]),
            }),
        )
        .await?;
    if json_output {
        print_value(&response, true)
    } else {
        println!("{}", response.id);
        Ok(())
    }
}

async fn run_schema(client: &Client, command: SchemaCommand, json_output: bool) -> Result<()> {
    let value: Value = client.get("/v1/schema").await?;
    let selected = match command {
        SchemaCommand::Export => value,
        SchemaCommand::Subjects => value
            .get("subjects")
            .cloned()
            .context("the schema response lacks subjects")?,
        SchemaCommand::Resources => value
            .get("resources")
            .cloned()
            .context("the schema response lacks resources")?,
        SchemaCommand::Claims { subject } => {
            let claims = value
                .get("claims")
                .and_then(Value::as_object)
                .context("the schema response lacks claims")?;
            if let Some(subject) = subject {
                let family = subject
                    .split_once('/')
                    .map(|(family, _)| family)
                    .context("a schema subject must be a full subject")?;
                Value::Object(
                    claims
                        .iter()
                        .filter(|(_, spec)| {
                            spec.get("subjects")
                                .and_then(Value::as_array)
                                .is_some_and(|subjects| {
                                    subjects.iter().any(|candidate| {
                                        candidate.as_str() == Some("*")
                                            || candidate.as_str() == Some(family)
                                    })
                                })
                        })
                        .map(|(kind, spec)| (kind.clone(), spec.clone()))
                        .collect(),
                )
            } else {
                Value::Object(claims.clone())
            }
        }
        SchemaCommand::Show { kind } => {
            let escaped = kind.replace('~', "~0").replace('/', "~1");
            value
                .pointer(&format!("/claims/{escaped}"))
                .or_else(|| value.pointer(&format!("/resources/{escaped}")))
                .or_else(|| value.pointer(&format!("/subjects/{escaped}")))
                .cloned()
                .with_context(|| format!("schema item `{kind}` is not registered"))?
        }
    };
    print_value(&selected, json_output)
}

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
                decision: decision.to_owned(),
                reason: args.reason,
                actor: Some(args.actor),
                expected_subject: None,
            },
        )
        .await?;
    print_value(&response, json_output)
}

async fn run_attention(
    client: &Client,
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    command: AttentionCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        AttentionCommand::Ls {
            actor,
            all,
            cursor,
            limit,
        } => {
            let actor = configured_human(actor.as_deref(), configured_person, "attention")?;
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the attention limit must be 1 through 200"
            );
            let response = generated_client(endpoint, Some(&actor))?
                .attention_list(cursor.as_deref(), Some(limit), all)
                .await?;
            let history = if all { " --all" } else { "" };
            print_product_page(
                &format!("HUMAN ATTENTION FOR {actor}"),
                &response,
                json_output,
                &format!("st attention ls --as {actor}{history}"),
            )
        }
        AttentionCommand::Show { subject, actor } => {
            let actor = configured_human(actor.as_deref(), configured_person, "attention")?;
            let normalized = normalize_member_subject(&subject, "attention");
            let path = format!("/v1/attention?person={}", urlencoding::encode(&actor));
            let item = client
                .get::<Vec<AttentionItemView>>(&path)
                .await?
                .into_iter()
                .find(|item| item.subject == normalized)
                .with_context(|| {
                    format!("attention item `{normalized}` is not currently actionable")
                })?;
            if json_output {
                print_value(&item, true)
            } else {
                print!(
                    "{}",
                    render_attention_show(&item, OutputStyle::stdout(), now_ms())
                );
                Ok(())
            }
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
                        until: args.until,
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
        AttentionCommand::Resolve(args) => {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let response: AttentionRequestView = client
                .post(
                    &format!(
                        "/v1/attention/resolve/{}",
                        urlencoding::encode(&args.subject)
                    ),
                    &AttentionResolveRequest {
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

async fn run_work(
    client: &Client,
    endpoint: &Endpoint,
    command: WorkCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        WorkCommand::Ls {
            actor,
            all,
            cursor,
            limit,
        } => {
            if let Some(actor) = actor.as_deref() {
                reject_foreign_agent_actor(actor)?;
            }
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the work limit must be 1 through 200"
            );
            let generated = generated_client(endpoint, None)?;
            let response = if let Some(actor) = actor.as_deref() {
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
        WorkCommand::Show { subject } => {
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

fn render_client_work_detail(work: &st3_client::Work) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "WORK  {}", work.header.id);
    let _ = writeln!(
        output,
        "{}  {}  attempt {} · readiness {}",
        work.state, work.path, work.attempt, work.readiness_epoch
    );
    let _ = writeln!(output, "Mission: {}", work.mission_run_id);
    let _ = writeln!(output, "Generation: {}", work.generation_id);
    let _ = writeln!(output, "Definition: {}", work.definition_id);
    if let Some(claimant) = &work.claimant {
        let _ = writeln!(output, "Claimant: {claimant}");
    }
    if let Some(incarnation) = &work.claim_incarnation {
        let _ = writeln!(output, "Incarnation: {incarnation}");
    }
    if let Some(reason) = &work.blocked_reason {
        // The store keeps the reason for any state change here, such as a failure or an
        // expired lease; only blocked work is blocked by it.
        let label = if work.state == "blocked" {
            "Blocked"
        } else {
            "Reason"
        };
        let _ = writeln!(output, "{label}: {reason}");
    }
    for blocker in &work.blockers {
        let _ = writeln!(output, "Blocker: {blocker}");
    }
    for goal in &work.goals {
        let _ = writeln!(output, "Goal: {goal}");
    }
    for constraint in &work.constraints {
        let _ = writeln!(output, "Constraint: {constraint}");
    }
    if let Some(usage) = &work.usage {
        let _ = writeln!(output, "Usage: {}", render_usage(usage));
    }
    if let Some(operational) = &work.header.operational {
        let reasons = if operational.reasons.is_empty() {
            "none".into()
        } else {
            operational.reasons.join(", ")
        };
        let _ = writeln!(
            output,
            "Operational: {} · actionable={} · reasons={reasons}",
            operational.layer, operational.actionable
        );
    }
    output
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
    let incarnation = match args.incarnation {
        Some(incarnation) => Some(incarnation),
        None => current_agent_incarnation(client, &actor).await?,
    };
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let response: StepRunView = client
        .post(
            &format!("/v1/work/{action}/{}", urlencoding::encode(&args.subject)),
            &WorkRequest {
                actor: Some(actor.clone()),
                incarnation,
                summary: args.summary,
                reason: args.reason,
                evidence: args.evidence,
                idempotency_key: format!("work:{action}:{}:{actor}:{nonce}", args.subject),
            },
        )
        .await?;
    if json_output {
        print_value(&response, true)
    } else {
        if action == "claim" {
            print!(
                "{}",
                render_step_run(&response, OutputStyle::stdout(), current_unix_ms()?)
            );
        } else {
            println!("{}\t{}", response.status, response.subject);
        }
        Ok(())
    }
}

async fn current_agent_incarnation(client: &Client, actor: &str) -> Result<Option<String>> {
    let subject = if actor.starts_with("agent/") {
        actor.to_owned()
    } else {
        format!("agent/{actor}")
    };
    let status: StatusResponse = client
        .get(&format!(
            "/v1/status?subject={}",
            urlencoding::encode(&subject)
        ))
        .await?;
    Ok(status
        .subjects
        .first()
        .and_then(|subject| subject.actual.as_ref())
        .and_then(|actual| actual.get("fields").unwrap_or(actual).get("incarnation_id"))
        .and_then(Value::as_str)
        .map(str::to_owned))
}

fn pty_observation_incarnation(
    actor: &str,
    observations: &[st_runtime::PtyObservation],
) -> Option<String> {
    let subject = if actor.starts_with("agent/") {
        actor
    } else {
        return None;
    };
    let observation = observations.iter().find(|observation| {
        observation.status == "running"
            && observation.tags.get("st3.subject").map(String::as_str) == Some(subject)
    })?;
    Some(format!(
        "{}:{}",
        observation.pid?,
        observation.created_at.as_deref()?
    ))
}

fn current_local_pty_incarnation(actor: &str) -> Result<Option<String>> {
    let Some(root) = std::env::var_os("PTY_ROOT").filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let observations = st_runtime::PtyRuntime::new(PathBuf::from(root))
        .snapshot()
        .context("reading the local PTY registry for the native driver incarnation")?;
    Ok(pty_observation_incarnation(actor, &observations))
}

async fn wait_for_agent_incarnation(client: &Client, actor: &str) -> Result<String> {
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut outage_logged = false;
    let has_local_pty_registry =
        std::env::var_os("PTY_ROOT").is_some_and(|value| !value.is_empty());
    loop {
        // A restarted provider process can begin before the reconciler has projected its new PTY
        // observation. Reading the graph immediately would then bind this new driver to the old
        // incarnation forever. The local registry already contains the process executing us and
        // is the exact source from which the reconciler will derive the graph incarnation.
        if has_local_pty_registry {
            if let Some(incarnation) = current_local_pty_incarnation(actor)? {
                return Ok(incarnation);
            }
        } else {
            match current_agent_incarnation(client, actor).await {
                Ok(Some(incarnation)) => return Ok(incarnation),
                Ok(None) => {}
                // A restarting daemon cannot answer yet; its outage does not use up the wait.
                Err(error) if st3::client::daemon_unreachable(&error).is_some() => {
                    if !outage_logged {
                        let _ = write_driver_log(
                            actor,
                            "waiting for the runtime incarnation while the daemon restarts",
                        );
                        outage_logged = true;
                    }
                    deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                }
                Err(error) => return Err(error),
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "the current runtime incarnation for `{actor}` did not appear within 15 seconds"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn message_page(
    client: &Client,
    recipient: Option<&str>,
    include_closed: bool,
    cursor: Option<&str>,
) -> Result<MessagePage> {
    let mut path = format!("/v1/messages/page?include_closed={include_closed}&limit=100");
    if let Some(recipient) = recipient {
        path.push_str(&format!("&to={}", urlencoding::encode(recipient)));
    }
    if let Some(cursor) = cursor {
        path.push_str(&format!("&cursor={}", urlencoding::encode(cursor)));
    }
    client.get(&path).await
}

async fn for_each_message(
    client: &Client,
    recipient: Option<&str>,
    include_closed: bool,
    mut visit: impl FnMut(MessageView) -> Result<()>,
) -> Result<()> {
    let mut cursor = None;
    loop {
        let page = message_page(client, recipient, include_closed, cursor.as_deref()).await?;
        for message in page.items {
            visit(message)?;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(()),
        }
    }
}

async fn run_message(
    client: &Client,
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    command: MessageCommand,
    json_output: bool,
) -> Result<()> {
    sync_message_projection(client).await?;
    match command {
        MessageCommand::Send(args) => {
            let Some(message) = send_message(client, args).await? else {
                return Ok(());
            };
            sync_message_projection(client).await?;
            if json_output {
                print_value(&message, true)
            } else {
                println!("{}", message.subject);
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
            for_each_message(client, Some(&identity), args.archive, |message| {
                if sender
                    .as_deref()
                    .is_some_and(|sender| sender != message.from)
                {
                    return Ok(());
                }
                count += 1;
                if args.count {
                    return Ok(());
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
                Ok(())
            })
            .await?;
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
                if messages.len() == 1 {
                    print_value(&messages[0], true)?;
                } else {
                    print_value(&messages, true)?;
                }
            } else {
                for (index, message) in messages.iter().enumerate() {
                    if index > 0 && !args.raw {
                        println!("\n---\n");
                    }
                    if args.raw {
                        print!("{}", message.content);
                        if index + 1 < messages.len() && !message.content.ends_with('\n') {
                            println!();
                        }
                    } else {
                        println!("Message: {}", message.subject);
                        println!("From: {}", message.from);
                        println!("To: {}", message.to);
                        if let Some(title) = &message.title {
                            println!("Subject: {title}");
                        }
                        println!();
                        println!("{}", message.content);
                    }
                }
            }
            sync_message_projection(client).await?;
            Ok(())
        }
        MessageCommand::Reply(args) => {
            let original = read_message(client, &args.reference).await?;
            let recipient = message_reply_recipient(&original, &args.from)?;
            let message = send_message(
                client,
                MessageSendArgs {
                    to: recipient,
                    body: args.body,
                    subject: args
                        .subject
                        .or(original.title.map(|title| format!("Re: {title}"))),
                    in_reply_to: Some(original.subject),
                    tags: Vec::new(),
                    from: args.from,
                    print_kdl: args.print_kdl,
                },
            )
            .await?;
            let Some(message) = message else {
                return Ok(());
            };
            if json_output {
                print_value(&message, true)
            } else {
                println!("{}", message.subject);
                Ok(())
            }
        }
        MessageCommand::Archive(args) => {
            let actor = args
                .actor
                .context("message archive needs explicit --as to record its lifecycle")?;
            reject_foreign_agent_actor(&actor)?;
            let mut claims = Vec::with_capacity(args.references.len());
            for reference in args.references {
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
        } => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the timeline limit must be 1 through 200"
            );
            let response = generated_client(endpoint, actor.as_deref().or(configured_person))?
                .timeline(&session, cursor.as_deref(), Some(limit))
                .await?;
            print_timeline_page(&response, json_output)
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

async fn send_message(client: &Client, args: MessageSendArgs) -> Result<Option<MessageView>> {
    let id = uuid::Uuid::now_v7().simple().to_string();
    let mission_id = format!("message/{id}");
    reject_foreign_agent_actor(&args.from)?;
    let from = normalize_message_subject(&args.from);
    let to = normalize_message_subject(&args.to);
    let kdl = message_mission_intent(
        &mission_id,
        &id,
        &from,
        &to,
        &args.body,
        args.subject.as_deref(),
        args.in_reply_to.as_deref(),
        &args.tags,
    );
    if args.print_kdl {
        print!("{kdl}");
        return Ok(None);
    }
    client
        .post(
            "/v1/messages",
            &MessageSendRequest {
                idempotency_key: format!("st3-message-send:{id}"),
                from,
                to,
                content: args.body,
                title: args.subject,
                in_reply_to: args.in_reply_to,
                tags: args.tags,
            },
        )
        .await
        .map(Some)
}

#[allow(clippy::too_many_arguments)]
fn message_mission_intent(
    mission_id: &str,
    message_id: &str,
    from: &str,
    to: &str,
    content: &str,
    title: Option<&str>,
    in_reply_to: Option<&str>,
    tags: &[String],
) -> String {
    let mut message = KdlNode::new("message");
    message.entries_mut().push(KdlEntry::new(message_id));
    let mut message_body = KdlDocument::new();
    message_body.nodes_mut().push(kdl_node("from", [from]));
    message_body.nodes_mut().push(kdl_node("to", [to]));
    message_body
        .nodes_mut()
        .push(kdl_node("content", [content]));
    if let Some(title) = title {
        message_body.nodes_mut().push(kdl_node("title", [title]));
    }
    if let Some(parent) = in_reply_to {
        message_body
            .nodes_mut()
            .push(kdl_node("in-reply-to", [parent]));
    }
    for tag in tags {
        message_body
            .nodes_mut()
            .push(kdl_node("tag", [tag.as_str()]));
    }
    message.set_children(message_body);

    let mut step = KdlNode::new("step");
    step.entries_mut().push(KdlEntry::new("send"));
    let mut step_body = KdlDocument::new();
    step_body.nodes_mut().push(KdlNode::new("agentless"));
    step_body.nodes_mut().push(message);
    step.set_children(step_body);

    let mut completion = KdlNode::new("completion");
    let mut completion_body = KdlDocument::new();
    completion_body
        .nodes_mut()
        .push(kdl_node("when", ["all-steps-exhausted"]));
    completion.set_children(completion_body);

    let mut mission = KdlNode::new("mission");
    mission.entries_mut().push(KdlEntry::new(mission_id));
    mission
        .entries_mut()
        .push(KdlEntry::new_prop("state", "ready"));
    let mut mission_body = KdlDocument::new();
    mission_body
        .nodes_mut()
        .push(kdl_node("goal", ["Deliver one message."]));
    mission_body.nodes_mut().push(step);
    mission_body.nodes_mut().push(completion);
    mission.set_children(mission_body);
    publication_document(mission)
}

async fn read_message(client: &Client, reference: &str) -> Result<MessageView> {
    let reference = normalize_message_reference(reference);
    client
        .get(&format!(
            "/v1/messages/read/{}",
            urlencoding::encode(&reference)
        ))
        .await
}

fn message_list_identity(explicit: Option<String>, ambient: Option<String>) -> Result<String> {
    explicit
        .filter(|identity| !identity.trim().is_empty())
        .or_else(|| ambient.filter(|identity| !identity.trim().is_empty()))
        .map(|identity| normalize_message_subject(&identity))
        .context(
            "conversations ls needs a mailbox identity argument or a non-empty ST_AGENT; refusing to list every fleet message",
        )
}

fn message_reply_recipient(original: &MessageView, sender: &str) -> Result<String> {
    let sender = normalize_message_subject(sender);
    if sender == original.from {
        Ok(original.to.clone())
    } else if sender == original.to {
        Ok(original.from.clone())
    } else {
        anyhow::bail!(
            "message `{}` is between `{}` and `{}`; `{sender}` cannot reply as a non-participant",
            original.subject,
            original.from,
            original.to
        )
    }
}

async fn read_message_after_lifecycle(
    client: &Client,
    reference: &str,
    actor: &str,
    archive: bool,
) -> Result<MessageView> {
    let message = read_message(client, reference).await?;
    let actor = normalize_message_subject(actor);
    if actor == message.from && actor != message.to {
        anyhow::ensure!(!archive, "a sender cannot archive the recipient's message");
        return Ok(message);
    }
    accept_message(client, &message, &actor).await?;
    if archive {
        close_message(client, reference, &actor).await?;
    }
    // Lifecycle writes are synchronous, so refetching makes JSON and other machine-readable
    // output describe the state that this command actually committed instead of its input state.
    read_message(client, &message.subject).await
}

async fn accept_message(client: &Client, message: &MessageView, actor: &str) -> Result<()> {
    let actor = normalize_message_subject(actor);
    anyhow::ensure!(
        actor == message.to,
        "message `{}` belongs to `{}`, not `{actor}`",
        message.subject,
        message.to
    );
    if !matches!(message.status.as_str(), "sent" | "staged" | "delivered") {
        return Ok(());
    }
    let reference = message.subject.trim_start_matches("message/");
    if matches!(message.status.as_str(), "sent" | "staged") {
        deliver_message(
            client,
            reference,
            &actor,
            format!("message-delivered-by-read:{}", message.subject),
        )
        .await?;
    }
    let _: ClaimRecord = client
        .post(
            &format!("/v1/messages/{}/claims", urlencoding::encode(reference)),
            &MessageLifecycleRequest {
                lifecycle: "read".into(),
                actor: Some(actor),
                transport: None,
                runtime_id: None,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: format!("message-read:{}", message.subject),
            },
        )
        .await?;
    Ok(())
}

async fn deliver_message(
    client: &Client,
    reference: &str,
    actor: &str,
    idempotency_key: String,
) -> Result<()> {
    let reference = normalize_message_reference(reference);
    let _: ClaimRecord = client
        .post(
            &format!("/v1/messages/{}/claims", urlencoding::encode(&reference)),
            &MessageLifecycleRequest {
                lifecycle: "delivered".into(),
                actor: Some(actor.into()),
                transport: None,
                runtime_id: None,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key,
            },
        )
        .await?;
    Ok(())
}

async fn stage_message(
    client: &Client,
    reference: &str,
    actor: &str,
    transport: &str,
    runtime_id: Option<&str>,
    idempotency_key: String,
) -> Result<()> {
    let reference = normalize_message_reference(reference);
    let _: ClaimRecord = client
        .post(
            &format!("/v1/messages/{}/claims", urlencoding::encode(&reference)),
            &MessageLifecycleRequest {
                lifecycle: "staged".into(),
                actor: Some(actor.into()),
                transport: Some(transport.into()),
                runtime_id: runtime_id.map(str::to_owned),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key,
            },
        )
        .await?;
    Ok(())
}

async fn close_message(client: &Client, reference: &str, actor: &str) -> Result<ClaimRecord> {
    let reference = normalize_message_reference(reference);
    client
        .post(
            &format!("/v1/messages/{}/claims", urlencoding::encode(&reference)),
            &MessageLifecycleRequest {
                lifecycle: "closed".into(),
                actor: Some(normalize_message_subject(actor)),
                transport: None,
                runtime_id: None,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: format!("message-closed:{reference}"),
            },
        )
        .await
}

fn normalize_message_reference(reference: &str) -> String {
    let file_reference = reference.ends_with(".md");
    let reference = if file_reference {
        Path::new(reference)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(reference)
            .trim_end_matches(".md")
    } else {
        reference
    };
    let reference = if file_reference {
        reference.split_once('-').map_or(reference, |(_, id)| id)
    } else {
        reference
    };
    let reference = urlencoding::decode(reference).unwrap_or(std::borrow::Cow::Borrowed(reference));
    reference.trim_start_matches("message/").to_owned()
}

async fn sync_message_projection(client: &Client) -> Result<()> {
    let Some(root) = std::env::var_os("ST3_MESSAGE_ROOT") else {
        return Ok(());
    };
    let mut export = st3::projection::MessageExport::new(Path::new(&root))?;
    for_each_message(client, None, true, |message| export.write(&message)).await?;
    export.finish()
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

struct TerminalScreen;

impl TerminalScreen {
    fn open() -> Result<Self> {
        print!("\x1b[?25l");
        std::io::stdout().flush()?;
        Ok(Self)
    }
}

impl Drop for TerminalScreen {
    fn drop(&mut self) {
        print!("\x1b[?25h");
        let _ = std::io::stdout().flush();
    }
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

#[allow(clippy::too_many_arguments)]
fn planning_session_intent(
    session_id: &str,
    mission_id: &str,
    request: &str,
    workspace: &Path,
    requester: &str,
    planner_spec: &PlannerSpec,
    target: Option<&MissionRunView>,
) -> String {
    let mut session = KdlNode::new("planning-session");
    session.entries_mut().push(KdlEntry::new(session_id));
    let mut body = KdlDocument::new();
    body.nodes_mut().push(kdl_node("mission", [mission_id]));
    body.nodes_mut().push(kdl_node("request", [request]));
    body.nodes_mut().push(kdl_node(
        "workspace",
        [workspace.to_string_lossy().as_ref()],
    ));
    body.nodes_mut().push(kdl_node("requester", [requester]));
    let mut planner = KdlNode::new("planner");
    planner
        .entries_mut()
        .push(KdlEntry::new(planner_spec.provider.as_str()));
    let mut planner_body = KdlDocument::new();
    if let Some(model) = planner_spec.model.as_deref() {
        planner_body.nodes_mut().push(kdl_node("model", [model]));
    }
    if let Some(effort) = planner_spec.effort.as_deref() {
        planner_body.nodes_mut().push(kdl_node("effort", [effort]));
    }
    planner.set_children(planner_body);
    body.nodes_mut().push(planner);
    if let Some(target) = target {
        body.nodes_mut()
            .push(kdl_node("target-run", [target.subject.as_str()]));
        body.nodes_mut()
            .push(kdl_node("target-generation", [target.generation.as_str()]));
    }
    session.set_children(body);
    publication_document(session)
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

fn parse_person_subject(actor: &str) -> std::result::Result<String, String> {
    if actor.starts_with("agent/") {
        // Agents reached for `now --as "$ST_AGENT"` and read the person-authority refusal as a
        // refusal of their own identity everywhere; name the agent commands instead.
        return Err(format!(
            "this option takes a person, not the agent `{actor}`; an agent lists its work with `st work ls --as \"$ST_AGENT\"` and its mail with `st conversations ls \"$ST_AGENT\"`"
        ));
    }
    let name = actor.strip_prefix("person/").ok_or_else(|| {
        "human authority must be explicit as a complete `person/NAME` subject".to_owned()
    })?;
    if name.is_empty() || name.contains('/') {
        return Err("human authority must be a complete `person/NAME` subject".into());
    }
    Ok(actor.to_owned())
}

fn parse_queue_move_actor(actor: &str) -> std::result::Result<String, String> {
    let parsed = if actor.starts_with("agent/") {
        parse_publication_actor(actor)
    } else {
        parse_person_subject(actor)
    };
    parsed.map_err(|_| "a queue move needs a complete `person/NAME` or `agent/PATH` subject".into())
}

fn parse_publication_actor(actor: &str) -> std::result::Result<String, String> {
    let valid = actor
        .strip_prefix("person/")
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
        || actor
            .strip_prefix("agent/")
            .is_some_and(|name| !name.is_empty() && !name.ends_with('/'));
    if !valid {
        return Err(
            "publication authority must be an explicit `person/NAME` or `agent/PATH` subject"
                .into(),
        );
    }
    Ok(actor.to_owned())
}

async fn run_driver(client: &Client, args: DriverArgs, catalog: Option<&Path>) -> Result<()> {
    if args.driver == "claude-mcp" {
        anyhow::ensure!(
            args.argv.is_empty(),
            "the Claude channel takes no provider argv"
        );
        let subject = args
            .subject
            .as_deref()
            .context("the Claude channel has no subject")?;
        let (catalog, _agent_dir, identity, _runtime_id) = prepare_native_driver(subject)?;
        return st2::claude_mcp::run_st3(&catalog, &identity);
    }
    if matches!(args.driver.as_str(), "pi-channel" | "omp-channel") {
        let identity = args
            .identity
            .as_deref()
            .context("the pi-family channel has no identity")?;
        let _ = catalog.context("the pi-family channel has no native driver catalog")?;
        anyhow::ensure!(
            args.argv.is_empty(),
            "the pi-family channel takes no provider argv"
        );
        let driver = if args.driver == "omp-channel" {
            "omp"
        } else {
            "pi"
        };
        return run_pi_channel(client, &normalize_agent_subject(identity), driver).await;
    }
    let subject = args
        .subject
        .as_deref()
        .context("the driver has no subject")?;
    if args.driver == "codex" {
        return run_codex_native(client, subject, args.argv).await;
    }
    if matches!(args.driver.as_str(), "claude" | "pi" | "omp" | "opencode") {
        return run_st2_native_driver(client, subject, &args.driver, args.argv).await;
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
) -> Result<()> {
    anyhow::ensure!(!argv.is_empty(), "the {driver} driver argv is empty");
    if driver == "claude" {
        reject_noninteractive_claude_argv(&argv)?;
    }
    let (catalog, agent_dir, identity, runtime_id) = prepare_native_driver(subject)?;
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
    let harness_state_path = st2::harness_state::harness_state_path(&agent_dir);
    let predecessor_harness_record = fs::read(&harness_state_path).ok();
    let mut current_harness_record_started = false;
    let task_catalog = catalog.clone();
    let task_agent = agent_dir.clone();
    let task_identity = identity.clone();
    let task_runtime = runtime_id.clone();
    let task_driver = driver.to_owned();
    let mut task = tokio::task::spawn_blocking(move || match task_driver.as_str() {
        "claude" => st2::claude_session::run_controlled_paths(
            &task_catalog,
            &task_agent,
            task_identity,
            task_runtime,
            argv,
        ),
        "pi" => st2::pi_session::run(&task_catalog, task_identity, task_runtime, argv),
        "omp" => st2::omp_session::run(&task_catalog, task_identity, task_runtime, argv),
        "opencode" => st2::opencode_session::run(&task_catalog, task_identity, task_runtime, argv),
        _ => unreachable!("the native driver was checked"),
    });
    let inbox = st2::message::inbox_dir(&agent_dir);
    let archive = st2::message::archive_dir(&agent_dir);
    // Mailbox projection reads the durable message history. A one-second poll
    // bounds delivery latency without repeatedly walking it four times a
    // second for every native harness during idle periods.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut work_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    work_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renewed_minute = None;
    let mut last_activity_fingerprint = None;
    let mut last_usage_fingerprint = None;
    let mut published_timeline = BTreeSet::new();
    let mut ready = false;
    let mut last_control_warning = None;
    let mut last_capacity_fingerprint = None;
    let mut delivery = NativeDeliverySupervisor::default();
    loop {
        tokio::select! {
            result = &mut task => {
                let outcome = result?;
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
                        Ok(_) => break,
                        Err(error) => {
                            tolerate_driver_api_outage(subject, error, &mut last_control_warning)?;
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                    }
                }
                return outcome;
            }
            _ = interval.tick() => {
                let tick: Result<()> = async {
                    let current_record = fs::read(&harness_state_path).ok();
                    current_harness_record_started = harness_record_belongs_to_current_session(
                        current_harness_record_started,
                        predecessor_harness_record.as_deref(),
                        current_record.as_deref(),
                    );
                    if native_file_may_override_channel(driver)
                        && current_harness_record_started
                        && let Some(observed) = st2::harness_state::read(&harness_state_path, None)
                    {
                        // A session claim is a startup fence, not an observation. Preserve the
                        // explicit `starting` state until a hook or the initialized ST3 channel
                        // supplies positive evidence; publishing the derived `claimed`
                        // indeterminacy would erase the more precise lifecycle state.
                        let claim_placeholder =
                            observed.state == st2::harness_state::Activity::Unknown
                                && observed.reason.as_deref() == Some("claimed");
                        if !claim_placeholder {
                            if !ready
                                && !matches!(
                                    observed.state,
                                    st2::harness_state::Activity::Unknown
                                        | st2::harness_state::Activity::Ended
                                )
                            {
                                let _: ClaimRecord = client.post("/v1/claims", &ClaimInput {
                                    subject: subject.into(),
                                    kind: "harness.observed".into(),
                                    actor: Some(subject.into()),
                                    fields: BTreeMap::from([
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
                                    ]),
                                    evidence: Vec::new(),
                                    expected_subject: None,
                                    idempotency_key: Some(format!("native-ready:{subject}:{driver}:{incarnation}")),
                                }).await?;
                                ready = true;
                            }
                            publish_harness_activity(
                                client,
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
                            publish_harness_usage(
                                client,
                                subject,
                                driver,
                                &incarnation,
                                &agent_dir,
                                &mut last_usage_fingerprint,
                            )
                            .await?;
                        }
                    }
                    let provider_incarnation = if current_harness_record_started {
                        st2::harness_state::read(&harness_state_path, None)
                            .and_then(|observed| observed.evidence_incarnation)
                    } else {
                        None
                    };
                    publish_harness_timeline(
                        client,
                        subject,
                        driver,
                        &incarnation,
                        provider_incarnation.as_deref(),
                        &agent_dir,
                        &mut published_timeline,
                    )
                    .await?;
                    if driver == "claude" {
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
                        supervise_native_delivery(
                            client,
                            subject,
                            &inbox,
                            &archive,
                            "opencode-server",
                            NativeDeliveryReceipts::OpenCode {
                                catalog_root: &catalog,
                                identity: &identity,
                                runtime_id: &runtime_id,
                            },
                            &incarnation,
                            &mut delivery,
                        )
                        .await;
                    }
                    Ok(())
                }.await;
                if let Err(error) = tick {
                    tolerate_driver_api_outage(subject, error, &mut last_control_warning)?;
                }
            }
            _ = work_interval.tick() => {
                let tick: Result<()> = async {
                    let minute = unix_minute()?;
                    if renewed_minute != Some(minute) {
                        renew_claimed_work(client, subject, minute).await?;
                        renewed_minute = Some(minute);
                    }
                    Ok(())
                }.await;
                if let Err(error) = tick {
                    tolerate_driver_api_outage(subject, error, &mut last_control_warning)?;
                }
            }
        }
    }
}

fn reject_noninteractive_claude_argv(argv: &[String]) -> Result<()> {
    let forbidden = argv.iter().find(|argument| {
        matches!(
            argument.as_str(),
            "-p" | "--print" | "--input-format" | "--output-format" | "--replay-user-messages"
        ) || argument.starts_with("--input-format=")
            || argument.starts_with("--output-format=")
    });
    anyhow::ensure!(
        forbidden.is_none(),
        "typed Claude harnesses always run the interactive TUI; `{}` is non-interactive, so use an `exec` declaration instead",
        forbidden.map(String::as_str).unwrap_or_default()
    );
    Ok(())
}

/// A provider task and its control loop start concurrently. Until the wrapper writes its session
/// claim, the harness-state path can still contain the predecessor's terminal record. Never
/// publish those bytes under the successor's runtime incarnation; the first byte change is the
/// wrapper's ownership fence, after which ownership sequencing prevents a predecessor rewrite.
fn harness_record_belongs_to_current_session(
    already_started: bool,
    predecessor: Option<&[u8]>,
    current: Option<&[u8]>,
) -> bool {
    already_started || current.is_some_and(|bytes| Some(bytes) != predecessor)
}

fn native_file_may_override_channel(driver: &str) -> bool {
    !matches!(driver, "pi" | "omp")
}

fn prepare_native_driver(subject: &str) -> Result<(PathBuf, PathBuf, String, String)> {
    let state_root = PathBuf::from(
        std::env::var_os("ST3_DRIVER_STATE_DIR")
            .context("the native driver has no ST3_DRIVER_STATE_DIR")?,
    );
    prepare_native_driver_in(subject, &state_root)
}

fn prepare_native_driver_in(
    subject: &str,
    state_root: &Path,
) -> Result<(PathBuf, PathBuf, String, String)> {
    let state_root = state_root.join(&hex::encode(Sha256::digest(subject.as_bytes()))[..24]);
    let catalog = state_root.join("catalog");
    fs::create_dir_all(&catalog)?;
    // This private catalog exists for st2 hook resolution, not for PTY ownership.
    // Its deeply nested state path would otherwise fail st2's portable socket-path
    // validation for slash-qualified st identities, silently disabling hooks.
    fs::write(
        catalog.join("catalog.kdl"),
        "catalog { pty-root \"/tmp/st3-native\" }\n",
    )?;
    let identity = subject.strip_prefix("agent/").unwrap_or(subject).to_owned();
    let host = st2::run::detect_host();
    let leaf = &hex::encode(Sha256::digest(identity.as_bytes()))[..16];
    let agent_dir = catalog.join("agents").join(&host).join(leaf);
    fs::create_dir_all(&agent_dir)?;
    let workspace = std::env::current_dir()?;
    let declaration = format!(
        "agent {identity:?} {{\n  identity {identity:?}\n  host {host:?}\n  workspace {:?}\n  command \"true\"\n}}\n",
        workspace.to_string_lossy()
    );
    fs::write(agent_dir.join("agent.kdl"), declaration)?;
    Ok((catalog, agent_dir, identity.clone(), identity))
}

fn harness_activity_state(activity: st2::harness_state::Activity) -> &'static str {
    match activity {
        st2::harness_state::Activity::Ready => "ready",
        st2::harness_state::Activity::Idle => "idle",
        st2::harness_state::Activity::Active | st2::harness_state::Activity::Child => "working",
        st2::harness_state::Activity::Ended => "ended",
        st2::harness_state::Activity::Unknown => "indeterminate",
    }
}

async fn publish_harness_activity(
    client: &Client,
    subject: &str,
    driver: &str,
    transport: &str,
    incarnation: Option<&str>,
    observed: &st2::harness_state::Observed,
    last_fingerprint: &mut Option<String>,
) -> Result<()> {
    let status = harness_activity_state(observed.state);
    let mut fields = BTreeMap::from([
        ("state".into(), Value::String(status.into())),
        ("driver".into(), Value::String(driver.into())),
        ("transport".into(), Value::String(transport.into())),
        (
            "blocked_on".into(),
            Value::String(observed.blocked_on.as_str().into()),
        ),
        ("ask".into(), Value::String(observed.ask.as_str().into())),
        (
            "input_buffer".into(),
            Value::String(observed.input_buffer.as_str().into()),
        ),
        (
            "reason".into(),
            observed
                .reason
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        ),
        (
            "exit".into(),
            observed
                .exit
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        ),
        (
            "observed_since_ms".into(),
            observed.since_ms.map(Value::from).unwrap_or(Value::Null),
        ),
        (
            "observed_at_ms".into(),
            observed
                .observed_at_ms
                .map(Value::from)
                .unwrap_or(Value::Null),
        ),
        (
            "ownership_sequence".into(),
            observed
                .ownership_sequence
                .map(Value::from)
                .unwrap_or(Value::Null),
        ),
        (
            "transition_sequence".into(),
            observed
                .transition_sequence
                .map(Value::from)
                .unwrap_or(Value::Null),
        ),
        (
            "evidence_incarnation".into(),
            observed
                .evidence_incarnation
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        ),
    ]);
    if let Some(incarnation) = incarnation {
        fields.insert("incarnation_id".into(), Value::String(incarnation.into()));
    }
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
        observed.since_ms,
        &fields,
    ))?));
    if last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
        return Ok(());
    }
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
                idempotency_key: Some(format!("native-activity:{subject}:{fingerprint}")),
            },
        )
        .await?;
    *last_fingerprint = Some(fingerprint);
    Ok(())
}

async fn publish_harness_usage(
    client: &Client,
    subject: &str,
    driver: &str,
    incarnation: &str,
    agent_dir: &Path,
    last_fingerprint: &mut Option<String>,
) -> Result<()> {
    let Some(observed) =
        st2::harness_context::read(&st2::harness_context::harness_context_path(agent_dir))
    else {
        return Ok(());
    };
    // A context-window occupancy reading and cumulative session spend are
    // different measurements. Publish them as distinct durable records; never
    // manufacture response-token buckets from occupancy.
    let mut readings = Vec::new();
    if observed.used_tokens.is_some()
        || observed.window_tokens.is_some()
        || observed.used_percent.is_some()
        || observed.compactions != 0
    {
        let mut fields = BTreeMap::from([
            (
                "semantics".into(),
                Value::String("context_occupancy".into()),
            ),
            ("driver".into(), Value::String(driver.into())),
            ("incarnation_id".into(), Value::String(incarnation.into())),
        ]);
        if let Some(value) = observed.used_tokens {
            fields.insert("context_used_tokens".into(), Value::from(value));
        }
        if let Some(value) = observed.window_tokens {
            fields.insert("context_window_tokens".into(), Value::from(value));
        }
        if let Some(value) = observed.used_percent {
            fields.insert("context_used_percent".into(), Value::from(value));
        }
        fields.insert("compactions".into(), Value::from(observed.compactions));
        if let Some(value) = observed.last_compaction_ms {
            fields.insert("last_compaction_ms".into(), Value::from(value));
        }
        let manually_requested = match observed.last_compaction_ms {
            Some(compacted_at) => {
                manual_compaction_request_matches(client, subject, incarnation, compacted_at)
                    .await
                    .unwrap_or(false)
            }
            None => false,
        };
        if manually_requested {
            fields.insert(
                "last_compaction_trigger".into(),
                Value::String("manual".into()),
            );
        } else if let Some(value) = &observed.last_compaction_trigger {
            fields.insert(
                "last_compaction_trigger".into(),
                Value::String(value.as_str().into()),
            );
        }
        if let Some(value) = &observed.model {
            fields.insert("model".into(), Value::String(value.clone()));
        }
        readings.push(fields);
    }
    if let Some(total) = observed.session_total_tokens {
        let mut fields = BTreeMap::from([
            (
                "semantics".into(),
                Value::String("session_cumulative".into()),
            ),
            ("driver".into(), Value::String(driver.into())),
            ("incarnation_id".into(), Value::String(incarnation.into())),
            ("total_tokens".into(), Value::from(total)),
        ]);
        if let Some(value) = observed.cost_usd {
            fields.insert("cost".into(), Value::from(value));
            fields.insert("currency".into(), Value::String("USD".into()));
        }
        if let Some(value) = &observed.model {
            fields.insert("model".into(), Value::String(value.clone()));
        }
        readings.push(fields);
    }
    if readings.is_empty() {
        return Ok(());
    }
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&readings)?));
    if last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
        return Ok(());
    }
    for fields in readings {
        let semantics = fields["semantics"].as_str().unwrap_or("unknown").to_owned();
        let _: ClaimRecord = client
            .post(
                "/v1/claims",
                &ClaimInput {
                    subject: subject.into(),
                    kind: "harness.usage".into(),
                    actor: Some(subject.into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!(
                        "harness-usage:{subject}:{incarnation}:{semantics}:{fingerprint}"
                    )),
                },
            )
            .await?;
    }
    *last_fingerprint = Some(fingerprint);
    Ok(())
}

async fn manual_compaction_request_matches(
    client: &Client,
    subject: &str,
    incarnation: &str,
    compacted_at_ms: u64,
) -> Result<bool> {
    const MANUAL_COMPACTION_WINDOW_MS: u128 = 5 * 60 * 1_000;
    let page: ClaimsPage = client
        .get(&format!(
            "/v1/claims?subject={}&order=desc&limit=200",
            urlencoding::encode(subject)
        ))
        .await?;
    let compacted_at_ms = u128::from(compacted_at_ms);
    let earliest = compacted_at_ms.saturating_sub(MANUAL_COMPACTION_WINDOW_MS);
    let latest = compacted_at_ms.saturating_add(5_000);
    let requests = page
        .claims
        .iter()
        .filter(|claim| {
            claim.kind == "terminal.input.requested"
                && claim.accepted_at_unix_ms >= earliest
                && claim.accepted_at_unix_ms <= latest
                && claim.body.pointer("/fields/intent").and_then(Value::as_str)
                    == Some("context-compaction")
                && claim
                    .body
                    .pointer("/fields/incarnation_id")
                    .and_then(Value::as_str)
                    == Some(incarnation)
        })
        .collect::<Vec<_>>();
    Ok(requests.iter().any(|request| {
        page.claims.iter().any(|claim| {
            claim.kind == "terminal.input.result"
                && claim.predecessors.iter().any(|id| id == &request.id)
                && claim.body.pointer("/fields/result").and_then(Value::as_str) == Some("written")
        })
    }))
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
        st2::harness_timeline::read(&st2::harness_timeline::timeline_path(agent_dir))
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
        let fields = timeline_claim_fields(operation, incarnation);
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&fields)?));
        let _: ClaimRecord = client
            .post(
                "/v1/claims",
                &ClaimInput {
                    subject: subject.into(),
                    kind: "harness.timeline".into(),
                    actor: Some(subject.into()),
                    fields,
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("harness-timeline:{subject}:{digest}")),
                },
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

fn timeline_record_is_current(
    record: &st2::harness_timeline::Record,
    driver: &str,
    provider_incarnation: Option<&str>,
) -> bool {
    record.driver == driver && provider_incarnation == Some(record.incarnation_id.as_str())
}

fn timeline_claim_fields(
    operation: st2::harness_timeline::Operation,
    runtime_incarnation: &str,
) -> BTreeMap<String, Value> {
    BTreeMap::from([
        (
            "operation".into(),
            Value::String(operation.operation.clone()),
        ),
        ("entry_id".into(), Value::String(operation.entry_id.clone())),
        ("sequence".into(), Value::from(operation.sequence)),
        ("revision".into(), Value::from(operation.revision)),
        ("role".into(), Value::String(operation.role)),
        ("entry_type".into(), Value::String(operation.entry_type)),
        ("final".into(), Value::Bool(operation.final_entry)),
        ("body".into(), operation.body),
        ("driver".into(), Value::String(operation.driver)),
        (
            "incarnation_id".into(),
            Value::String(runtime_incarnation.into()),
        ),
        (
            "observed_at_unix_ms".into(),
            Value::from(operation.observed_at_unix_ms),
        ),
    ])
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

/// One pi-family message frame. The content is the shared `<smalltalk-message>` envelope that
/// Codex also receives, steered into a running turn at its next tool boundary. omp backgrounds an in-flight
/// shell or eval call when a steer arrives, and one live omp seat then repeated a send whose
/// result it had not seen. Queueing mail with `followUp` instead was measured and was worse: omp
/// read the queued messages during its turn, the queue then re-delivered them as new prompts, and
/// seats that answered those stale prompts declined the next real task in three of six
/// cross-harness runs.
fn pi_family_message_frame(message: &st3::model::MessageView, body: &str, identity: &str) -> Value {
    json!({
        "type": "message",
        "deliverAs": "steer",
        "content": st2::ding::st3_notification_text(
            &message.subject,
            &message.from,
            &message.to,
            message.title.as_deref(),
            body,
            &st2::ding::st3_body_sha256(body),
        ),
        "meta": {
            "from": message.from,
            "messageId": message.subject,
            "threadId": message.in_reply_to.clone().unwrap_or_else(|| message.subject.clone()),
            "identity": identity,
        },
    })
}

/// Session-start context for pi-family seats. It restates the st boot contract only: st has no
/// availability or busy status, so st2 status vocabulary sends the model searching for commands
/// that do not exist before it claims ready work. It also names the seat, because omp's Python
/// tool runs with a filtered environment that drops `ST_AGENT` and `ST3_BIN`; a model that probes
/// there first must not infer its identity from the fleet listing.
fn pi_family_session_ritual(subject: &str) -> String {
    format!(
        "Follow .st3/boot.md now. You are `{subject}`; your shell tool also has it as `$ST_AGENT` and the st executable as `$ST3_BIN`. Read and archive handled graph messages, then list, claim, do, and finish your ready st work."
    )
}

async fn run_pi_channel(client: &Client, subject: &str, driver: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    let incarnation = wait_for_agent_incarnation(client, subject).await?;
    let identity = subject.strip_prefix("agent/").unwrap_or(subject);
    let context_name = format!("doc/context/{identity}/now");
    let context =
        retry_while_daemon_unreachable(subject, || latest_document_text(client, &context_name))
            .await?
            .unwrap_or_default();
    let ritual = pi_family_session_ritual(subject);
    let session_context = if context.trim().is_empty() {
        ritual
    } else {
        format!(
            "<context source=\"st3/context/now.md\" agent=\"{identity}\">\n{}\n</context>\n\n{ritual}",
            context.trim_end()
        )
    };
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "type": "hello",
                    "protocol": 1,
                    "identity": identity,
                    "sessionContext": session_context,
                }))?
            )
            .as_bytes(),
        )
        .await?;
    stdout.flush().await?;

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut delivered = BTreeSet::new();
    let mut failed_handoffs = BTreeMap::<String, u32>::new();
    let mut failed_diagnostics = BTreeSet::new();
    let mut first_idle_seen = false;
    let mut last_warning = None;
    let mut work_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    work_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renewed_minute = None;
    let mut frame_sequence = 0_u64;
    let session = std::env::var("ST2_PI_CHANNEL_SESSION").unwrap_or_else(|_| "unknown".into());
    // Reports the daemon has not accepted yet. A restart must not end the channel or lose the
    // harness's latest state, so each waits here and is sent again on the next tick.
    let mut pending = PiFamilyReports::default();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { return Ok(()); };
                let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue; };
                match frame.get("type").and_then(Value::as_str) {
                    Some("state") => {
                        let Some(state) = frame.get("state").and_then(Value::as_str) else { continue; };
                        let status = match state {
                            "active" => "working",
                            "idle" => { first_idle_seen = true; "idle" },
                            _ => continue,
                        };
                        frame_sequence = frame_sequence.saturating_add(1);
                        pending.state = Some((status, frame_sequence));
                    }
                    Some("delivered") => {
                        let Some(message) = frame.pointer("/meta/messageId").and_then(Value::as_str) else { continue; };
                        pending.acknowledgements.insert(message.to_owned());
                    }
                    Some("failed") => {
                        let Some(message) = frame.pointer("/meta/messageId").and_then(Value::as_str) else { continue; };
                        let failures = failed_handoffs.entry(message.to_owned()).or_default();
                        *failures += 1;
                        if *failures < 3 {
                            delivered.remove(message);
                        } else {
                            failed_diagnostics.insert(message.to_owned());
                        }
                        continue;
                    }
                    _ => continue,
                }
                if let Err(error) = pending
                    .publish(client, subject, driver, &incarnation, &session)
                    .await
                {
                    warn_pi_channel(subject, &error, &mut last_warning);
                }
            }
            _ = interval.tick() => {
                if let Err(error) = pending
                    .publish(client, subject, driver, &incarnation, &session)
                    .await
                {
                    warn_pi_channel(subject, &error, &mut last_warning);
                }
                for message in failed_diagnostics.clone() {
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
                        Ok(_) => { failed_diagnostics.remove(&message); },
                        Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
                    }
                }
                if !first_idle_seen { continue; }
                let mut cursor = None;
                loop {
                    let page = match message_page(client, Some(subject), false, cursor.as_deref()).await {
                        Ok(page) => page,
                        Err(error) => { warn_pi_channel(subject, &error, &mut last_warning); break; }
                    };
                    for message in page.items.into_iter().filter(|message| matches!(message.status.as_str(), "sent" | "staged")) {
                    if !delivered.insert(message.subject.clone()) {
                        continue;
                    }
                    let body = match message_content(client, &message).await {
                        Ok(body) => body,
                        Err(error) => {
                            delivered.remove(&message.subject);
                            warn_pi_channel(subject, &error, &mut last_warning);
                            continue;
                        }
                    };
                    if message.status == "sent" {
                        match stage_pi_family_message(client, &message.subject, subject, driver).await {
                            Ok(true) => {},
                            Ok(false) => { delivered.remove(&message.subject); continue; },
                            Err(error) => {
                                delivered.remove(&message.subject);
                                warn_pi_channel(subject, &error, &mut last_warning);
                                continue;
                            }
                        }
                    }
                    let frame = pi_family_message_frame(&message, &body, identity);
                    stdout.write_all(serde_json::to_string(&frame)?.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                    }
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
            }
            _ = work_interval.tick() => {
                let minute = unix_minute()?;
                if renewed_minute != Some(minute) {
                    match renew_claimed_work(client, subject, minute).await {
                        Ok(()) => renewed_minute = Some(minute),
                        Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
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

/// Harness reports a pi-family channel owes the daemon.
#[derive(Default)]
struct PiFamilyReports {
    /// Only the latest state matters; a newer frame replaces an unsent older one.
    state: Option<(&'static str, u64)>,
    acknowledgements: BTreeSet<String>,
}

impl PiFamilyReports {
    async fn publish(
        &mut self,
        client: &Client,
        subject: &str,
        driver: &str,
        incarnation: &str,
        session: &str,
    ) -> Result<()> {
        if let Some((status, sequence)) = self.state {
            let _: ClaimRecord = client
                .post(
                    "/v1/claims",
                    &ClaimInput {
                        subject: subject.into(),
                        kind: "harness.observed".into(),
                        actor: Some(subject.into()),
                        fields: BTreeMap::from([
                            ("state".into(), Value::String(status.into())),
                            ("driver".into(), Value::String(driver.into())),
                            (
                                "transport".into(),
                                Value::String(format!("{driver}-channel")),
                            ),
                            ("incarnation_id".into(), Value::String(incarnation.into())),
                        ]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!(
                            "pi-state:{subject}:{incarnation}:{session}:{sequence}"
                        )),
                    },
                )
                .await?;
            self.state = None;
        }
        while let Some(message) = self.acknowledgements.first().cloned() {
            acknowledge_pi_family_delivery(client, subject, &message).await?;
            self.acknowledgements.remove(&message);
        }
        Ok(())
    }
}

async fn stage_pi_family_message(
    client: &Client,
    message: &str,
    subject: &str,
    driver: &str,
) -> Result<bool> {
    match stage_message(
        client,
        message,
        subject,
        &format!("{driver}-channel"),
        None,
        format!("native-staged:{driver}-channel:{subject}:{message}"),
    )
    .await
    {
        Ok(_) => Ok(true),
        Err(error) => match read_message(client, message).await {
            Ok(view) if view.status == "staged" => Ok(true),
            Ok(view) if matches!(view.status.as_str(), "delivered" | "read" | "closed") => {
                Ok(false)
            }
            _ => Err(error),
        },
    }
}

/// Record that a pi-family harness took one message.
///
/// The recipient can read a message through the CLI before its channel acknowledges the handoff,
/// and the omp channel holds mail until a tool batch returns, so the acknowledgement can arrive
/// after the message has moved past delivery. Such a message needs no acknowledgement. Failing
/// here would end the channel and leave the seat with no mail or state
/// (cross-omp-hold-astra-20260927-a).
async fn acknowledge_pi_family_delivery(
    client: &Client,
    subject: &str,
    message: &str,
) -> Result<()> {
    let Err(error) = deliver_message(
        client,
        message,
        subject,
        format!("pi-delivered:{subject}:{message}"),
    )
    .await
    else {
        return Ok(());
    };
    match read_message(client, message).await {
        Ok(view) if matches!(view.status.as_str(), "delivered" | "read" | "closed") => Ok(()),
        _ => Err(error),
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

async fn message_content(client: &Client, message: &MessageView) -> Result<String> {
    if message.content.starts_with("doc/") {
        let value: Value = client
            .get(&format!(
                "/v1/documents/content?reference={}",
                urlencoding::encode(&message.content)
            ))
            .await?;
        let bytes = serde_json::from_value::<Vec<u8>>(
            value
                .get("bytes")
                .cloned()
                .context("document response lacks bytes")?,
        )?;
        String::from_utf8(bytes).context("message document is not UTF-8")
    } else {
        Ok(message.content.clone())
    }
}

async fn run_codex_native(client: &Client, subject: &str, argv: Vec<String>) -> Result<()> {
    anyhow::ensure!(!argv.is_empty(), "the Codex driver argv is empty");
    let incarnation = wait_for_agent_incarnation(client, subject).await?;
    let (catalog, agent_dir, identity, runtime_id) = prepare_native_driver(subject)?;
    let root = catalog
        .parent()
        .context("the Codex driver catalog has no state root")?
        .to_path_buf();
    let state_dir = root.join("state");
    let prior_binding = std::fs::read(state_dir.join("binding.json")).ok();
    let inbox = st2::message::inbox_dir(&agent_dir);
    let archive = st2::message::archive_dir(&agent_dir);
    let driver_root = catalog.clone();
    let driver_state = state_dir.clone();
    let driver_agent = agent_dir.clone();
    let driver_identity = identity.clone();
    let driver_runtime_id = runtime_id.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        st2::codex_app_server::run_controlled_paths(
            &driver_root,
            &driver_state,
            &driver_agent,
            driver_identity,
            driver_runtime_id,
            argv,
        )
    });
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut work_interval = tokio::time::interval(std::time::Duration::from_secs(1));
    work_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renewed_minute = None;
    let mut ready = false;
    let mut last_activity_fingerprint = None;
    let mut last_usage_fingerprint = None;
    let mut published_timeline = BTreeSet::new();
    let mut last_control_warning = None;
    let mut last_capacity_fingerprint = None;
    let mut delivery = NativeDeliverySupervisor::default();
    loop {
        tokio::select! {
            result = &mut task => {
                let outcome = result.context("joining the Codex driver")?;
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
            _ = interval.tick() => {
                let tick: Result<()> = async {
                    if !ready && std::fs::read(state_dir.join("binding.json"))
                        .ok()
                        .is_some_and(|binding| Some(&binding) != prior_binding.as_ref())
                    {
                        let _: ClaimRecord = client.post("/v1/claims", &ClaimInput {
                            subject: subject.into(),
                            kind: "harness.observed".into(),
                            actor: Some(subject.into()),
                            fields: BTreeMap::from([
                                ("state".into(), Value::String("ready".into())),
                                ("driver".into(), Value::String("codex".into())),
                                ("transport".into(), Value::String("app-server".into())),
                                ("incarnation_id".into(), Value::String(incarnation.clone())),
                            ]),
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: Some(format!("codex-ready:{subject}:{incarnation}")),
                        }).await?;
                        ready = true;
                    }
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
                    if let Some(observed) = st2::harness_state::read(
                        &st2::harness_state::harness_state_path(&agent_dir),
                        None,
                    ) {
                        publish_harness_activity(
                            client,
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
                    publish_harness_usage(
                        client,
                        subject,
                        "codex",
                        &incarnation,
                        &agent_dir,
                        &mut last_usage_fingerprint,
                    )
                    .await?;
                    publish_harness_timeline(
                        client,
                        subject,
                        "codex",
                        &incarnation,
                        Some(&incarnation),
                        &agent_dir,
                        &mut published_timeline,
                    )
                    .await?;
                    Ok(())
                }.await;
                if let Err(error) = tick {
                    tolerate_driver_api_outage(subject, error, &mut last_control_warning)?;
                }
            }
            _ = work_interval.tick() => {
                let tick: Result<()> = async {
                    let minute = unix_minute()?;
                    if renewed_minute != Some(minute) {
                        renew_claimed_work(client, subject, minute).await?;
                        renewed_minute = Some(minute);
                    }
                    Ok(())
                }.await;
                if let Err(error) = tick {
                    tolerate_driver_api_outage(subject, error, &mut last_control_warning)?;
                }
            }
        }
    }
}

const PROVIDER_CAPACITY_MAX_RETRIES: u32 = 6;
const PROVIDER_CAPACITY_BASE_BACKOFF_MS: u64 = 30_000;
const PROVIDER_CAPACITY_MAX_BACKOFF_MS: u64 = 600_000;

fn provider_capacity_backoff_ms(subject: &str, incarnation: &str, attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(20);
    let base = PROVIDER_CAPACITY_BASE_BACKOFF_MS
        .saturating_mul(1_u64 << shift)
        .min(PROVIDER_CAPACITY_MAX_BACKOFF_MS);
    let digest = Sha256::digest(format!("{subject}:{incarnation}:{attempt}").as_bytes());
    let seed = u16::from_be_bytes([digest[0], digest[1]]) as u64;
    let jitter = seed % (base.saturating_div(4).saturating_add(1));
    base.saturating_add(jitter)
}

async fn publish_provider_capacity_diagnostic(
    client: &Client,
    subject: &str,
    incarnation: &str,
    observed_since_ms: Option<u64>,
    event_fingerprint: &str,
) -> Result<()> {
    let claims: ClaimsPage = client
        .get(&format!(
            "/v1/claims?subject={}&order=desc&limit=100",
            urlencoding::encode(subject)
        ))
        .await?;
    let capacity_claims = claims.claims.iter().filter(|claim| {
        let fields = claim.body.get("fields").unwrap_or(&claim.body);
        claim.kind == "harness.diagnostic"
            && fields.get("code").and_then(Value::as_str) == Some("provider-capacity")
            && fields.get("incarnation_id").and_then(Value::as_str) == Some(incarnation)
    });
    if observed_since_ms.is_some_and(|observed_since_ms| {
        capacity_claims.clone().any(|claim| {
            claim
                .body
                .pointer("/fields/observed_since_ms")
                .and_then(Value::as_u64)
                == Some(observed_since_ms)
        })
    }) {
        return Ok(());
    }
    let attempt = capacity_claims
        .filter_map(|claim| {
            claim
                .body
                .pointer("/fields/retry_attempt")
                .and_then(Value::as_u64)
                .and_then(|attempt| u32::try_from(attempt).ok())
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    let retryable = attempt <= PROVIDER_CAPACITY_MAX_RETRIES;
    let mut fields = BTreeMap::from([
        (
            "severity".into(),
            Value::String(if retryable { "warning" } else { "error" }.into()),
        ),
        (
            "status".into(),
            Value::String(if retryable { "waiting" } else { "failed" }.into()),
        ),
        ("code".into(), Value::String("provider-capacity".into())),
        (
            "reason".into(),
            Value::String(if retryable {
                "the selected model is temporarily at capacity; st will retry this session".into()
            } else {
                "the selected model remained at capacity after the automatic retry limit".into()
            }),
        ),
        ("incarnation_id".into(), Value::String(incarnation.into())),
        ("retry_attempt".into(), Value::from(attempt)),
    ]);
    if let Some(observed_since_ms) = observed_since_ms {
        fields.insert("observed_since_ms".into(), Value::from(observed_since_ms));
    }
    if retryable {
        let retry_after = u64::try_from(current_unix_ms()?)
            .unwrap_or(u64::MAX)
            .saturating_add(provider_capacity_backoff_ms(subject, incarnation, attempt));
        fields.insert("retry_after_unix_ms".into(), Value::from(retry_after));
    }
    let work: Vec<StepRunView> = client
        .get(&format!("/v1/work?actor={}", urlencoding::encode(subject)))
        .await?;
    if let Some(step) = work.into_iter().find(|step| {
        matches!(step.status.as_str(), "claimed" | "working")
            && step.claimant.as_deref() == Some(subject)
            && step.claim_incarnation.as_deref() == Some(incarnation)
    }) {
        fields.insert("step_run".into(), Value::String(step.subject));
    }
    let _: ClaimRecord = client
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: subject.into(),
                kind: "harness.diagnostic".into(),
                actor: Some(subject.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!(
                    "provider-capacity:{subject}:{incarnation}:{event_fingerprint}"
                )),
            },
        )
        .await?;
    Ok(())
}

fn tolerate_driver_api_outage(
    subject: &str,
    error: anyhow::Error,
    last_warning: &mut Option<Instant>,
) -> Result<()> {
    let outage = st3::client::daemon_unreachable(&error).map(|outage| outage.summary());
    let transient = outage.is_some()
        || error.chain().any(|cause| {
            let message = cause.to_string();
            message.contains("incomplete HTTP response")
                || message.contains("retry the command")
                || cause
                    .downcast_ref::<serde_json::Error>()
                    .is_some_and(serde_json::Error::is_eof)
                || cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::UnexpectedEof
                    )
                })
        });
    if !transient {
        return Err(error);
    }
    let now = Instant::now();
    if last_warning.is_none_or(|prior| now.duration_since(prior) >= Duration::from_secs(10)) {
        // The driver shares a PTY with its provider. Writing to stderr here would
        // corrupt the provider's interactive screen while the API is restarting.
        let line = format!(
            "{}; the driver keeps running and retries every second until the daemon is back",
            outage.unwrap_or_else(|| format!("the st daemon did not answer ({error:#})"))
        );
        let _ = write_driver_log(subject, &line);
        *last_warning = Some(now);
    }
    Ok(())
}

/// Repeat one driver call until the daemon answers. A driver outlives daemon restarts, so an
/// outage while it starts delays the seat instead of ending it.
async fn retry_while_daemon_unreachable<T, F, Fut>(subject: &str, mut call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_warning = None;
    loop {
        match call().await {
            Ok(value) => return Ok(value),
            Err(error) => tolerate_driver_api_outage(subject, error, &mut last_warning)?,
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Driver notes go to a private log, never to the terminal the driver shares with its provider.
fn write_driver_log(subject: &str, line: &str) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;

    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("no state directory for driver warning")?;
    let directory = state_home.join("st3");
    fs::create_dir_all(&directory)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(directory.join("driver-api-warnings.log"))?;
    let at = current_unix_ms()?;
    writeln!(file, "{at} {subject} {}", line.replace('\n', " "))?;
    Ok(())
}

fn work_incarnation_key(incarnation: Option<&str>) -> String {
    incarnation.map_or_else(
        || "unknown".into(),
        |value| hex::encode(Sha256::digest(value.as_bytes()))[..12].to_owned(),
    )
}

async fn renew_claimed_work(client: &Client, subject: &str, minute: u64) -> Result<()> {
    let status: StatusResponse = client
        .get(&format!(
            "/v1/status?subject={}",
            urlencoding::encode(subject)
        ))
        .await?;
    let harness = status
        .subjects
        .iter()
        .find(|candidate| candidate.subject == subject)
        .and_then(|candidate| candidate.harness.as_ref());
    let work: Vec<StepRunView> = client
        .get(&format!("/v1/work?actor={}", urlencoding::encode(subject)))
        .await?;
    for step in work
        .into_iter()
        .filter(|step| work_claim_has_active_harness(step, subject, harness))
    {
        let _: StepRunView = client
            .post(
                &format!("/v1/work/renew/{}", urlencoding::encode(&step.subject)),
                &WorkRequest {
                    actor: Some(subject.into()),
                    incarnation: step.claim_incarnation,
                    summary: None,
                    reason: None,
                    evidence: Vec::new(),
                    idempotency_key: format!("native-renew:{}:{subject}:{minute}", step.subject),
                },
            )
            .await?;
    }
    Ok(())
}

fn work_claim_has_active_harness(
    step: &StepRunView,
    subject: &str,
    harness: Option<&CurrentHarnessView>,
) -> bool {
    matches!(
        step.status.as_str(),
        "claimed" | "working" | "verifying" | "blocked"
    ) && step.claimant.as_deref() == Some(subject)
        && harness.is_some_and(|harness| {
            harness.state != "ended"
                && step.claim_incarnation.as_deref() == Some(harness.incarnation_id.as_str())
        })
}

fn unix_minute() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() / 60)
}

fn current_unix_ms() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

/// Native delivery is a supervised part of the driver, not a reason to terminate the provider.
/// Each attempt rereads graph message state, so a failed page or receipt is replayed safely.
struct NativeDeliverySupervisor {
    episode: u64,
    /// Whether the current failure episode began with the daemon unreachable.
    daemon_outage: bool,
    failures: u32,
    retry_after: Option<Instant>,
    degraded_recorded: bool,
    last_warning: Option<Instant>,
}

impl Default for NativeDeliverySupervisor {
    fn default() -> Self {
        Self {
            episode: 0,
            daemon_outage: false,
            failures: 0,
            retry_after: None,
            degraded_recorded: false,
            last_warning: None,
        }
    }
}

impl NativeDeliverySupervisor {
    fn ready(&self) -> bool {
        self.retry_after
            .is_none_or(|retry_after| Instant::now() >= retry_after)
    }

    /// Back off a failing delivery, except while the daemon is unreachable: a refused connect
    /// costs nothing, and delivery should resume within a second of the daemon's return.
    fn failed(&mut self, daemon_unreachable: bool) -> Duration {
        if self.failures == 0 {
            self.episode = self.episode.saturating_add(1);
            self.daemon_outage = daemon_unreachable;
        }
        self.failures = self.failures.saturating_add(1);
        let shift = self.failures.saturating_sub(1).min(5);
        let backoff = if daemon_unreachable {
            Duration::from_secs(1)
        } else {
            Duration::from_secs((1_u64 << shift).min(30))
        };
        self.retry_after = Some(Instant::now() + backoff);
        backoff
    }

    fn recovered(&mut self) {
        self.failures = 0;
        self.retry_after = None;
        self.degraded_recorded = false;
        self.last_warning = None;
    }
}

async fn record_native_delivery_diagnostic(
    client: &Client,
    subject: &str,
    incarnation: &str,
    transport: &str,
    episode: u64,
    daemon_outage: bool,
    recovered: bool,
) -> Result<()> {
    // The harness.diagnostic schema admits only warning and error severities and no transport
    // field; a recovery is a warning whose status is `recovered`, as repairs record it.
    let (code, status, reason) = if recovered {
        (
            "native-delivery-recovered",
            "recovered",
            format!(
                "Native conversation delivery over {transport} recovered and resumed replay from durable graph state."
            ),
        )
    } else if daemon_outage {
        (
            "native-delivery-degraded",
            "waiting",
            format!(
                "Native conversation delivery over {transport} paused while the st daemon was unreachable; the driver stayed online and retried every second."
            ),
        )
    } else {
        (
            "native-delivery-degraded",
            "waiting",
            format!(
                "Native conversation delivery over {transport} failed; the driver remains online and will retry with bounded backoff."
            ),
        )
    };
    let _: ClaimRecord = client
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: subject.into(),
                kind: "harness.diagnostic".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("severity".into(), Value::String("warning".into())),
                    ("status".into(), Value::String(status.into())),
                    ("code".into(), Value::String(code.into())),
                    ("reason".into(), Value::String(reason)),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!(
                    "{code}:{subject}:{incarnation}:{transport}:{episode}"
                )),
            },
        )
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn supervise_native_delivery(
    client: &Client,
    subject: &str,
    inbox: &Path,
    archive: &Path,
    transport: &str,
    receipts: NativeDeliveryReceipts<'_>,
    incarnation: &str,
    supervisor: &mut NativeDeliverySupervisor,
) {
    if !supervisor.ready() {
        return;
    }
    match forward_projected_messages(client, subject, inbox, archive, transport, receipts).await {
        Ok(()) => {
            if supervisor.failures == 0 {
                return;
            }
            // If the API was unavailable during the failure, publish both transitions now.
            if !supervisor.degraded_recorded {
                supervisor.degraded_recorded = record_native_delivery_diagnostic(
                    client,
                    subject,
                    incarnation,
                    transport,
                    supervisor.episode,
                    supervisor.daemon_outage,
                    false,
                )
                .await
                .is_ok();
            }
            if supervisor.degraded_recorded
                && record_native_delivery_diagnostic(
                    client,
                    subject,
                    incarnation,
                    transport,
                    supervisor.episode,
                    supervisor.daemon_outage,
                    true,
                )
                .await
                .is_ok()
            {
                // The driver shares its provider's terminal; the graph diagnostic is the record.
                let _ = write_driver_log(subject, "native conversation delivery resumed");
                supervisor.recovered();
            }
        }
        Err(error) => {
            let outage = st3::client::daemon_unreachable(&error).map(|outage| outage.summary());
            let backoff = supervisor.failed(outage.is_some());
            let now = Instant::now();
            if supervisor
                .last_warning
                .is_none_or(|prior| now.duration_since(prior) >= Duration::from_secs(10))
            {
                let line = match outage {
                    Some(outage) => format!(
                        "native conversation delivery paused: {outage}. Messages stay queued in the graph; retrying every {}s until the daemon is back",
                        backoff.as_secs()
                    ),
                    None => format!(
                        "native conversation delivery failed; retrying in {}s with backoff up to 30s: {error:#}",
                        backoff.as_secs()
                    ),
                };
                let _ = write_driver_log(subject, &line);
                supervisor.last_warning = Some(now);
            }
            if !supervisor.degraded_recorded {
                supervisor.degraded_recorded = record_native_delivery_diagnostic(
                    client,
                    subject,
                    incarnation,
                    transport,
                    supervisor.episode,
                    supervisor.daemon_outage,
                    false,
                )
                .await
                .is_ok();
            }
        }
    }
}

async fn forward_projected_messages(
    client: &Client,
    subject: &str,
    inbox: &Path,
    archive: &Path,
    transport: &str,
    receipts: NativeDeliveryReceipts<'_>,
) -> Result<()> {
    const TAG_PREFIX: &str = "st3-message:";
    let mut present = projected_message_files(inbox, archive)?;
    let mut consumed_by_recipient = BTreeSet::new();
    let mut active_subjects = BTreeSet::new();
    let stage_runtime_id = match receipts {
        NativeDeliveryReceipts::Codex { runtime_id, .. }
        | NativeDeliveryReceipts::OpenCode { runtime_id, .. } => Some(runtime_id),
        NativeDeliveryReceipts::ClaudeChannel { .. } => None,
    };
    let consumed = match receipts {
        NativeDeliveryReceipts::Codex {
            state_dir,
            identity,
            runtime_id,
        } => st2::codex_app_server::consumed_delivery_filenames(state_dir, identity, runtime_id),
        NativeDeliveryReceipts::ClaudeChannel {
            agent_dir,
            incarnation,
        } => claude_channel_consumed_delivery_filenames(agent_dir, incarnation),
        NativeDeliveryReceipts::OpenCode {
            catalog_root,
            identity,
            runtime_id,
        } => st2::opencode_session::consumed_delivery_filenames(catalog_root, identity, runtime_id),
    }?;
    let mut cursor = None;
    loop {
        let page = message_page(client, Some(subject), false, cursor.as_deref()).await?;
        for message in page.items {
            active_subjects.insert(message.subject.clone());
            if matches!(message.status.as_str(), "read" | "closed") {
                consumed_by_recipient.insert(message.subject);
                continue;
            }
            if !matches!(message.status.as_str(), "sent" | "staged") {
                continue;
            }
            let filename = if let Some(filename) = present.get(&message.subject) {
                filename.clone()
            } else {
                let content = if message.content.starts_with("doc/") {
                    let value: Value = client
                        .get(&format!(
                            "/v1/documents/content?reference={}",
                            urlencoding::encode(&message.content)
                        ))
                        .await?;
                    let bytes = serde_json::from_value::<Vec<u8>>(
                        value
                            .get("bytes")
                            .cloned()
                            .context("document response lacks bytes")?,
                    )?;
                    String::from_utf8(bytes).context("message document is not UTF-8")?
                } else {
                    message.content.clone()
                };
                let mut tags = message.tags.clone();
                tags.push(format!("{TAG_PREFIX}{}", message.subject));
                tags.push(format!("{}{}", st2::ding::ST3_TO_TAG, message.to));
                tags.push(format!(
                    "{}{}",
                    st2::ding::ST3_SHA256_TAG,
                    st2::ding::st3_body_sha256(&content)
                ));
                let filename = st2::message::send_to_inbox(
                    inbox,
                    &message.from,
                    message.title.as_deref(),
                    message.in_reply_to.as_deref(),
                    &tags,
                    &content,
                )?;
                present.insert(message.subject.clone(), filename.clone());
                filename
            };
            if message.status == "sent" {
                stage_message(
                    client,
                    &message.subject,
                    subject,
                    transport,
                    stage_runtime_id,
                    format!("native-staged:{transport}:{subject}:{}", message.subject),
                )
                .await?;
            }
            // Receipt-backed transports advance graph delivery only after their durable ledger proves
            // that the exact inbox file was consumed by a provider turn. Materialization alone is
            // merely queued native delivery.
            if !native_delivery_receipted(&consumed, &filename) {
                continue;
            }
            deliver_message(
                client,
                &message.subject,
                subject,
                format!("native-delivered:{transport}:{subject}:{}", message.subject),
            )
            .await?;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    // A message can close between polls. The active page intentionally excludes
    // history, so inspect only projected files still in the native inbox before
    // deciding whether to archive them. A failed lookup keeps the file in place.
    for file in st2::message::list_dir(inbox)? {
        for reference in file
            .tags
            .iter()
            .filter_map(|tag| tag.strip_prefix(TAG_PREFIX))
        {
            if active_subjects.contains(reference) || consumed_by_recipient.contains(reference) {
                continue;
            }
            if let Ok(message) = read_message(client, reference).await
                && message.to == subject
                && matches!(message.status.as_str(), "read" | "closed")
            {
                consumed_by_recipient.insert(reference.to_owned());
            }
        }
    }
    sync_consumed_projected_messages(inbox, archive, &consumed_by_recipient)?;
    Ok(())
}

#[derive(Clone, Copy)]
enum NativeDeliveryReceipts<'a> {
    Codex {
        state_dir: &'a Path,
        identity: &'a str,
        runtime_id: &'a str,
    },
    /// The interactive channel writes a marker into the synthetic user prompt. Only Claude's
    /// `UserPromptSubmit` hook can project it into this exact incarnation's durable timeline.
    ClaudeChannel {
        agent_dir: &'a Path,
        incarnation: &'a str,
    },
    OpenCode {
        catalog_root: &'a Path,
        identity: &'a str,
        runtime_id: &'a str,
    },
}

fn claude_channel_consumed_delivery_filenames(
    agent_dir: &Path,
    incarnation: &str,
) -> Result<BTreeSet<String>> {
    const PREFIX: &str = "[st3-delivery:";
    let Some(record) =
        st2::harness_timeline::read(&st2::harness_timeline::timeline_path(agent_dir))
    else {
        return Ok(BTreeSet::new());
    };
    if record.driver != "claude" || record.incarnation_id != incarnation {
        return Ok(BTreeSet::new());
    }
    Ok(record
        .operations
        .iter()
        .filter(|operation| operation.role == "user" && operation.entry_type == "content")
        .filter_map(|operation| operation.body.get("text").and_then(Value::as_str))
        .flat_map(|text| text.split(PREFIX).skip(1))
        .filter_map(|tail| tail.split_once(']').map(|(filename, _)| filename))
        .filter(|filename| st2::message::is_message_filename(filename))
        .map(str::to_owned)
        .collect())
}

fn native_exit_key(subject: &str, runtime_id: &str, incarnation: &str) -> String {
    format!("native-exit:{subject}:{runtime_id}:{incarnation}")
}

fn claude_receipt_incarnation<'a>(
    _runtime_incarnation: &str,
    provider_incarnation: Option<&'a str>,
) -> &'a str {
    // Claude's hook timeline is fenced by its provider session token, which differs from
    // the PTY runtime incarnation used for st claims.
    provider_incarnation.unwrap_or_default()
}

fn native_delivery_receipted(consumed: &BTreeSet<String>, filename: &str) -> bool {
    consumed.contains(filename)
}

fn sync_consumed_projected_messages(
    inbox: &Path,
    archive: &Path,
    consumed_by_recipient: &BTreeSet<String>,
) -> Result<()> {
    const TAG_PREFIX: &str = "st3-message:";
    for message in st2::message::list_dir(inbox)? {
        let is_consumed = message
            .tags
            .iter()
            .filter_map(|tag| tag.strip_prefix(TAG_PREFIX))
            .any(|subject| consumed_by_recipient.contains(subject));
        if is_consumed {
            st2::message::archive_msg(inbox, archive, &message.filename)?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn projected_message_subjects(inbox: &Path, archive: &Path) -> Result<BTreeSet<String>> {
    Ok(projected_message_files(inbox, archive)?
        .into_keys()
        .collect())
}

fn projected_message_files(inbox: &Path, archive: &Path) -> Result<BTreeMap<String, String>> {
    const TAG_PREFIX: &str = "st3-message:";
    Ok(st2::message::list_dir(inbox)?
        .into_iter()
        .chain(st2::message::list_dir(archive)?)
        .flat_map(|message| {
            message.tags.into_iter().filter_map(move |tag| {
                tag.strip_prefix(TAG_PREFIX)
                    .map(|subject| (subject.to_owned(), message.filename.clone()))
            })
        })
        .collect())
}

fn read_intent(path: Option<&Path>) -> Result<(String, Option<String>)> {
    match path {
        Some(path) if path != Path::new("-") => Ok((
            fs::read_to_string(path).with_context(|| format!("read KDL {}", path.display()))?,
            Some(path.display().to_string()),
        )),
        _ => {
            let mut source = String::new();
            std::io::stdin().read_to_string(&mut source)?;
            Ok((source, None))
        }
    }
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

fn print_value(value: &impl serde::Serialize, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        let value = serde_json::to_value(value)?;
        print!("{}", render_human_value(&value, OutputStyle::stdout()));
    }
    Ok(())
}

fn idempotency(kdl: &str, tokens: &BTreeMap<String, Vec<String>>) -> String {
    let mut hash = Sha256::new();
    hash.update(kdl.as_bytes());
    hash.update(serde_json::to_vec(tokens).expect("tokens serialize"));
    hex::encode(hash.finalize())
}

/// Trim the local observation log at startup and then once an hour. Local observations
/// never replicate, so this never changes what any peer holds.
async fn trim_local_observations(store: Arc<Store>, observations: st3::config::ObservationsConfig) {
    const LOCAL_OBSERVATION_TRIM_INTERVAL: Duration = Duration::from_secs(60 * 60);
    const LOCAL_OBSERVATION_TRIM_CHUNK: usize = 5_000;
    let retention_ms = observations
        .retention_ms()
        .expect("the daemon validated its observation retention");
    loop {
        let store = store.clone();
        let max_per_subject_kind = observations.max_per_subject_kind;
        let trimmed = tokio::task::spawn_blocking(move || {
            store.trim_local_observations(
                now_ms().saturating_sub(u128::from(retention_ms)),
                max_per_subject_kind,
                LOCAL_OBSERVATION_TRIM_CHUNK,
            )
        })
        .await;
        match trimmed {
            Ok(Ok(0)) => {}
            Ok(Ok(count)) => eprintln!("st3: trimmed {count} local observations"),
            Ok(Err(error)) => eprintln!("st3: local observation trim failed: {error:#}"),
            Err(error) => eprintln!("st3: local observation trim stopped: {error}"),
        }
        tokio::time::sleep(LOCAL_OBSERVATION_TRIM_INTERVAL).await;
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn parse_field(value: &str) -> Result<(String, Value), String> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| "a field must use KEY=VALUE".to_owned())?;
    if key.is_empty() {
        return Err("a field key is empty".into());
    }
    let value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into()));
    Ok((key.into(), value))
}

fn parse_peer(value: &str) -> Result<PeerConfig, String> {
    let (name, url) = value.split_once('=').unwrap_or((value, ""));
    if name.is_empty() {
        return Err("a peer needs a name".to_owned());
    }
    Ok(PeerConfig {
        name: name.into(),
        url: url.into(),
    })
}

fn parse_input(value: &str) -> Result<(String, String), String> {
    let (name, value) = value
        .split_once('=')
        .ok_or_else(|| "a mission input must use NAME=VALUE".to_owned())?;
    if name.is_empty() || name.contains('/') || name.chars().any(char::is_whitespace) {
        return Err("a mission input name is invalid".into());
    }
    Ok((name.into(), value.into()))
}

fn parse_launch_decision_type(value: &str) -> Result<LaunchDecisionType, String> {
    serde_json::from_value(Value::String(value.to_owned())).map_err(|_| {
        "decision type must be boolean, single-choice, multiple-choice, or rank".into()
    })
}

fn parse_launch_decision_option(value: &str) -> Result<LaunchDecisionOption, String> {
    serde_json::from_str(value)
        .map_err(|error| format!("invalid structured decision option: {error}"))
}

fn parse_launch_decision_response(value: &str) -> Result<LaunchDecisionResponse, String> {
    serde_json::from_str(value)
        .map_err(|error| format!("invalid structured decision response: {error}"))
}

fn unique_pairs(values: Vec<(String, String)>, kind: &str) -> Result<BTreeMap<String, String>> {
    let mut output = BTreeMap::new();
    for (name, value) in values {
        anyhow::ensure!(
            output.insert(name.clone(), value).is_none(),
            "the {kind} `{name}` repeats"
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn peer_status(peer: &str, digest: Option<&str>) -> st3::model::ReplicationPeerStatus {
        st3::model::ReplicationPeerStatus {
            peer: peer.into(),
            status: "up".into(),
            last_success_at_unix_ms: None,
            last_error: None,
            schema_digest: None,
            authority_digest: digest.map(str::to_owned),
            graph_digest: None,
            sync: None,
        }
    }

    #[test]
    fn a_leave_is_confirmed_by_a_matching_digest_or_a_refusal_as_left() {
        let mut status = ReplicationStatus {
            authority_digest: "mine".into(),
            peers: vec![peer_status("a", Some("theirs")), peer_status("c", None)],
            ..Default::default()
        };
        assert_eq!(leave_confirmation(&status, None).unwrap(), None);
        assert!(leave_peer_summary(&status).contains("a up (different digest)"));

        // A member that admitted the leave refuses this node and never reports its digest.
        let left = st3::config::FleetRemoval {
            reported_by: "a".into(),
            code: "member-left".into(),
        };
        assert_eq!(
            leave_confirmation(&status, Some(&left)).unwrap(),
            Some("a".into())
        );

        // A removal is no confirmation: writes after it do not replicate.
        let removed = st3::config::FleetRemoval {
            reported_by: "c".into(),
            code: "member-removed".into(),
        };
        let error = leave_confirmation(&status, Some(&removed)).unwrap_err();
        assert!(error.to_string().contains("--offline"), "{error}");

        status.peers[1].authority_digest = Some("mine".into());
        assert_eq!(leave_confirmation(&status, None).unwrap(), Some("c".into()));
    }

    #[test]
    fn missions_tree_fixture_renders_all_sections() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/missions-tree.json"))
                .expect("valid missions tree fixture");
        assert_eq!(
            render_missions_tree(&fixture),
            include_str!("../tests/fixtures/missions-tree.txt")
        );
        let cli = Cli::try_parse_from(["st", "missions", "tree", "--json"])
            .expect("missions tree --json parses");
        assert!(cli.json);
        assert!(matches!(
            cli.command,
            Command::Missions {
                command: MissionViewCommand::Tree
            }
        ));
    }

    #[test]
    fn conversations_ls_accepts_the_read_spelling_of_its_mailbox() {
        let cli = Cli::try_parse_from(["st3", "conversations", "ls", "--as", "agent/run-1/worker"])
            .expect("conversations ls --as parses");
        let Command::Conversations {
            command: MessageCommand::Ls(args),
        } = cli.command
        else {
            panic!("conversations ls")
        };
        assert_eq!(args.actor.as_deref(), Some("agent/run-1/worker"));
        assert!(
            Cli::try_parse_from(["st3", "conversations", "ls", "agent/a", "--as", "agent/b"])
                .is_err()
        );
    }

    #[test]
    fn pi_family_mail_uses_the_shared_envelope_and_the_steer_boundary() {
        let message = st3::model::MessageView {
            subject: "message/0123456789abcdef".into(),
            from: "agent/run-1/wake.claude".into(),
            to: "agent/run-1/wake.omp-2".into(),
            content: "FACT QUARTZ".into(),
            status: "sent".into(),
            title: Some("Cross-harness consensus: idle".into()),
            in_reply_to: None,
            tags: vec![],
            created_index: 1,
        };
        let omp = pi_family_message_frame(&message, "FACT QUARTZ", "run-1/wake.omp-2");
        assert_eq!(omp["deliverAs"], "steer");
        assert_eq!(
            omp["content"],
            format!(
                "<smalltalk-message id=\"0123456789abcdef\" from=\"agent/run-1/wake.claude\" \
                 to=\"agent/run-1/wake.omp-2\" subject=\"Cross-harness consensus: idle\" \
                 sha256=\"{}\" graph=\"message/0123456789abcdef\">\nFACT QUARTZ\n</smalltalk-message>",
                st2::ding::st3_body_sha256("FACT QUARTZ")
            )
        );
        assert_eq!(omp["meta"]["messageId"], "message/0123456789abcdef");
    }

    #[test]
    fn a_person_option_points_an_agent_to_its_own_commands() {
        let refusal = parse_person_subject("agent/run-1/worker").unwrap_err();
        assert!(refusal.contains("takes a person, not the agent `agent/run-1/worker`"));
        assert!(refusal.contains("work ls --as"));
        assert_eq!(
            parse_person_subject("person/operator").as_deref(),
            Ok("person/operator")
        );
        assert!(parse_person_subject("operator").is_err());
    }

    #[test]
    fn a_harness_cannot_act_as_another_agent() {
        let own = Some("agent/run-1/wake.omp-2");
        let run = Some("run-1");
        let refusal = foreign_agent_actor("agent/run-1/wake.codex", own, run).unwrap();
        assert!(refusal.contains("this harness is `agent/run-1/wake.omp-2`"));
        assert!(foreign_agent_actor("wake.codex", own, run).is_some());
        assert!(foreign_agent_actor("agent/run-1/wake.omp-2", own, run).is_none());
        assert!(foreign_agent_actor("wake.omp-2", own, run).is_none());
        // Non-agent actors and processes without a seat identity are not seat impersonation.
        assert!(foreign_agent_actor("person/eval-requester", own, run).is_none());
        assert!(foreign_agent_actor("requester", own, run).is_none());
        assert!(foreign_agent_actor("exec/run-1/controller", own, run).is_none());
        assert!(foreign_agent_actor("agent/run-1/wake.codex", None, run).is_none());
        assert!(
            foreign_agent_actor("agent/run-1/wake.codex", Some("person/operator"), run).is_none()
        );
    }

    #[test]
    fn mission_start_requires_an_explicit_actor() {
        assert!(Cli::try_parse_from(["st3", "missions", "start", "mission/demo"]).is_err());
    }

    #[test]
    fn a_harness_cannot_mutate_as_a_peer_or_person() {
        let cases: &[&[&str]] = &[
            &[
                "st3",
                "missions",
                "publish",
                "mission.kdl",
                "--as",
                "agent/peer",
            ],
            &[
                "st3",
                "missions",
                "start",
                "mission/demo",
                "--as",
                "person/operator",
            ],
            &[
                "st3",
                "agents",
                "queue",
                "move",
                "agent/worker",
                "mission-run/demo/one",
                "--top",
                "--as",
                "agent/peer",
            ],
            &[
                "st3",
                "work",
                "revision",
                "approve",
                "revision-proposal/x",
                "hash",
                "--as",
                "person/operator",
            ],
            &["st3", "work", "wake", "step-run/x/y", "--as", "agent/peer"],
            &[
                "st3",
                "diagnostic",
                "--as",
                "person/operator",
                "--code",
                "test",
                "--reason",
                "test",
            ],
        ];
        for arguments in cases {
            let cli = Cli::try_parse_from(*arguments).unwrap();
            assert!(
                guard_mutating_cli_actor(&cli.command, Some("agent/own"), None).is_err(),
                "accepted {arguments:?}"
            );
        }
        let own = Cli::try_parse_from(["st3", "work", "wake", "step-run/x/y", "--as", "agent/own"])
            .unwrap();
        assert!(guard_mutating_cli_actor(&own.command, Some("agent/own"), None).is_ok());
        assert!(guard_mutating_cli_actor(&own.command, None, None).is_ok());
    }

    #[test]
    fn pi_family_session_ritual_uses_only_the_st3_boot_contract() {
        let ritual = pi_family_session_ritual("agent/fleet/example/omp");
        assert!(ritual.contains("You are `agent/fleet/example/omp`"));
        let ritual = ritual.to_ascii_lowercase();
        assert!(ritual.contains(".st3/boot.md"));
        for st2_vocabulary in ["status", "available", "busy", "st2"] {
            assert!(
                !ritual.contains(st2_vocabulary),
                "the pi-family session ritual mentions `{st2_vocabulary}`"
            );
        }
    }

    #[test]
    fn agent_card_shows_the_member_reconcile_fault() {
        let agent: st3_client::Agent = serde_json::from_value(serde_json::json!({
            "kind": "agent", "id": "agent/bad", "revision": "one",
            "updated_at": "2026-09-27T20:04:00Z", "name": "Bad",
            "state": "failed", "reachability": "local", "runtime_ids": [],
            "fault": "render refuses to change tracked file .claude/settings.local.json"
        }))
        .unwrap();
        let card = render_client_agent(&agent, &[], 0);
        assert!(card.contains("STATE        failed"));
        assert!(card.contains(
            "FAULT        render refuses to change tracked file .claude/settings.local.json"
        ));
    }

    #[test]
    fn agent_card_shows_current_and_next_work_ids() {
        let resource: st3_client::Resource = serde_json::from_value(serde_json::json!({
            "kind": "agent", "id": "agent/worker", "revision": "one",
            "updated_at": "2026-09-24T09:00:00Z", "name": "Worker",
            "state": "running", "reachability": "local", "runtime_ids": [],
            "current_work_ids": ["step-run/older/work"], "active_work_count": 1,
            "next_work_id": "step-run/newer/review",
            "upcoming_work_ids": ["step-run/newer/review"], "queued_work_count": 1
        }))
        .unwrap();
        let st3_client::Resource::Agent(agent) = resource else {
            panic!("agent resource")
        };
        let card = render_client_agent(&agent, &[], 0);
        assert!(card.contains("CURRENT WORK step-run/older/work"));
        assert!(card.contains("NEXT WORK    step-run/newer/review"));
        assert!(
            !card.contains("PROGRESS"),
            "an unreadable step leaves only its id"
        );
    }

    #[test]
    fn agent_card_shows_the_current_step_and_its_last_progress() {
        let resource: st3_client::Resource = serde_json::from_value(serde_json::json!({
            "kind": "agent", "id": "agent/worker", "revision": "one",
            "updated_at": "2026-09-24T09:00:00Z", "name": "Worker",
            "state": "running", "reachability": "local", "runtime_ids": [],
            "current_work_ids": ["step-run/one/build", "step-run/two/review", "step-run/two/docs"],
            "active_work_count": 3
        }))
        .unwrap();
        let st3_client::Resource::Agent(agent) = resource else {
            panic!("agent resource")
        };
        let step = |subject: &str, title: &str, progress: Option<(&str, u128)>| {
            serde_json::from_value::<StepRunView>(serde_json::json!({
                "subject": subject, "run": "mission-run/demo", "generation": "run-generation/one",
                "step": subject.rsplit('/').next().unwrap(), "definition_hash": "hash",
                "status": "working", "attempt": 1, "assigned_to": "agent/worker",
                "agentless": false, "title": title, "worker_reported": false,
                "claimant": "agent/worker", "claim_incarnation": "worker:1",
                "claim_expires_at_unix_ms": 900_000, "readiness_epoch": 1,
                "blocked_reason": null, "not_before_unix_ms": null,
                "created_at_unix_ms": 0, "updated_at_unix_ms": 0,
                "progress_summary": progress.map(|(summary, _)| summary),
                "progress_at_unix_ms": progress.map(|(_, at)| at),
            }))
            .unwrap()
        };
        let current = [
            step(
                "step-run/one/build",
                "Build the parser",
                Some(("Tests pass\nnext: docs", 60_000)),
            ),
            step("step-run/two/review", "Review the parser", None),
            StepRunView {
                status: "verifying".into(),
                completion_summary: Some("Published the guide".into()),
                ..step(
                    "step-run/two/docs",
                    "Write the guide",
                    Some(("Drafting", 0)),
                )
            },
        ];

        let card = render_client_agent(&agent, &current, 360_000);

        assert!(card.contains(
            "CURRENT WORK step-run/one/build\n\
             CURRENT STEP Build the parser · working\n\
             PROGRESS     Tests pass… · 5m ago\n\
             CURRENT WORK step-run/two/review\n\
             CURRENT STEP Review the parser · working\n\
             PROGRESS     none reported\n\
             CURRENT WORK step-run/two/docs\n\
             CURRENT STEP Write the guide · verifying\n\
             DONE         Published the guide\n"
        ));
    }

    #[test]
    fn agent_queue_view_lists_the_claim_then_runs_in_order_and_moves() {
        let queue: st3_client::AgentQueue = serde_json::from_value(serde_json::json!({
            "kind": "agent-queue", "agent_id": "agent/fleet/worker",
            "current_work_ids": ["step-run/held/build"],
            "next_work_id": "step-run/second/review",
            "runs": [
                {
                    "mission_run_id": "mission-run/held", "position": 1, "state": "claimed",
                    "run_state": "running", "joined_at": "2026-09-24T09:00:00.000Z",
                    "claimed_work_ids": ["step-run/held/build"], "ready_work_ids": [],
                    "waiting_work_ids": []
                },
                {
                    "mission_run_id": "mission-run/gated", "position": 2, "state": "waiting",
                    "run_state": "running", "joined_at": "2026-09-24T09:01:00.000Z",
                    "claimed_work_ids": [], "ready_work_ids": [],
                    "waiting_work_ids": ["step-run/gated/ship"]
                },
                {
                    "mission_run_id": "mission-run/second", "position": 3, "state": "ready",
                    "run_state": "running", "joined_at": "2026-09-24T09:02:00.000Z",
                    "claimed_work_ids": [],
                    "ready_work_ids": ["step-run/second/review", "step-run/second/docs"],
                    "waiting_work_ids": []
                }
            ],
            "moves": [{
                "claim_id": "claim-one", "mission_run_id": "mission-run/held",
                "placement": "before", "anchor_run_id": "mission-run/gated",
                "actor_id": "person/operator", "reason": "finish the build first",
                "moved_at": "2026-09-24T09:03:00.000Z"
            }],
            "move_count": 1
        }))
        .unwrap();
        assert_eq!(
            render_agent_queue(&queue),
            "AGENT QUEUE  agent/fleet/worker\n\
             CURRENT      step-run/held/build\n\
             NEXT WORK    step-run/second/review\n\
             RUNS         3\n  \
             1. mission-run/held  claimed  step-run/held/build\n  \
             2. mission-run/gated  waiting  step-run/gated/ship not ready\n  \
             3. mission-run/second  ready  next step-run/second/review (+1 ready)\n\
             MOVES        1 total\n  \
             2026-09-24T09:03:00.000Z  person/operator moved mission-run/held before \
             mission-run/gated: finish the build first\n"
        );
    }

    #[tokio::test]
    async fn a_pi_family_delivery_after_the_recipient_read_the_message_keeps_the_channel() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let store = Arc::new(Store::open_memory("pi-delivery-test").unwrap());
        let seat = "agent/run-1/wake.omp";
        let claim =
            |subject: &str, kind: &str, actor: &str, fields: Vec<(&str, &str)>| ClaimInput {
                subject: subject.into(),
                kind: kind.into(),
                actor: Some(actor.into()),
                fields: fields
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), Value::String(value.into())))
                    .collect(),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: None,
            };
        for subject in ["message/held", "message/pending"] {
            store
                .append_claim(&claim(
                    subject,
                    "message.sent",
                    "agent/run-1/wake.codex",
                    vec![
                        ("from", "agent/run-1/wake.codex"),
                        ("to", seat),
                        ("content", "AGREEMENT EMBER+ORBIT"),
                        ("status", "sent"),
                    ],
                ))
                .unwrap();
        }
        // The seat read the held message through the CLI, which records delivery and the read.
        for lifecycle in ["delivered", "read"] {
            store
                .append_claim(&claim(
                    "message/held",
                    &format!("message.{lifecycle}"),
                    seat,
                    vec![("status", lifecycle)],
                ))
                .unwrap();
        }
        let state = AppState {
            store: store.clone(),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "pi-delivery-test".into(),
            state_dir: root.path().to_path_buf(),
            pty_root: root.path().join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: PlannerSpec::default(),
        };
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            serve_unix(&server_socket, router(state)).await.unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(socket.exists(), "the test API socket did not start");
        let client = Client::unix(&socket);

        // The channel can lose a race to the recipient's CLI read without ending its loop.
        assert!(
            !stage_pi_family_message(&client, "message/held", seat, "omp")
                .await
                .unwrap()
        );
        assert!(
            stage_pi_family_message(&client, "message/pending", seat, "omp")
                .await
                .unwrap()
        );
        assert_eq!(
            store.message("message/pending").unwrap().unwrap().status,
            "staged"
        );

        // The late acknowledgement itself is still an invalid transition...
        assert!(
            deliver_message(&client, "message/held", seat, "late".into())
                .await
                .is_err()
        );
        // ...but it must not end the channel.
        acknowledge_pi_family_delivery(&client, seat, "message/held")
            .await
            .unwrap();
        assert_eq!(
            store.message("message/held").unwrap().unwrap().status,
            "read"
        );
        acknowledge_pi_family_delivery(&client, seat, "message/pending")
            .await
            .unwrap();
        assert_eq!(
            store.message("message/pending").unwrap().unwrap().status,
            "delivered"
        );
        // A message that does not exist is still an error.
        assert!(
            acknowledge_pi_family_delivery(&client, seat, "message/absent")
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn trace_after_index_reads_the_first_bounded_page() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let store = Arc::new(Store::open_memory("trace-cursor-test").unwrap());
        let indexes = (0..5)
            .map(|number| {
                store
                    .append_claim(&ClaimInput {
                        subject: "host/trace-cursor-test".into(),
                        kind: "transport.observed".into(),
                        actor: None,
                        fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: Some(format!("trace-cursor-test-{number}")),
                    })
                    .unwrap()
                    .store_index
            })
            .collect::<Vec<_>>();
        let state = AppState {
            store,
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "trace-cursor-test".into(),
            state_dir: root.path().to_path_buf(),
            pty_root: root.path().join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: PlannerSpec::default(),
        };
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            serve_unix(&server_socket, router(state)).await.unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(socket.exists(), "the test API socket did not start");
        let client = Client::unix(&socket);
        let mut args = TraceArgs {
            subject: Some("host/trace-cursor-test".into()),
            owner_run: None,
            limit: 2,
            after_index: Some(indexes[0]),
            follow: false,
        };

        let after = trace_claims(&client, &args).await.unwrap();
        assert_eq!(
            after
                .iter()
                .map(|claim| claim.store_index)
                .collect::<Vec<_>>(),
            indexes[1..3]
        );

        args.after_index = None;
        let recent = trace_claims(&client, &args).await.unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|claim| claim.store_index)
                .collect::<Vec<_>>(),
            indexes[3..5]
        );
        server.abort();
    }

    #[test]
    fn conversation_follow_is_explicit_and_bounded() {
        let cli = Cli::try_parse_from([
            "st3",
            "conversations",
            "follow",
            "session/remote-agent",
            "--limit",
            "25",
        ])
        .unwrap();
        let Command::Conversations {
            command:
                MessageCommand::Follow {
                    session,
                    actor,
                    limit,
                },
        } = cli.command
        else {
            panic!("the conversation follow command did not parse");
        };
        assert_eq!(session, "session/remote-agent");
        assert_eq!(actor, None);
        assert_eq!(limit, 25);
    }

    #[test]
    fn conversation_follow_emits_new_entries_and_revisions_once() {
        let entry = |id: &str, sequence: u64, revision: u32| {
            serde_json::from_value::<ClientTimelineEntry>(json!({
                "id": id,
                "sequence": sequence,
                "revision": revision,
                "timestamp": "2026-09-24T15:00:00Z",
                "role": "assistant",
                "final": true,
                "type": "content",
                "body": {"media_type":"text/plain","text":"hello","attachment_id":null}
            }))
            .unwrap()
        };
        let mut seen = BTreeMap::new();
        let initial = vec![entry("timeline-entry/a", 1, 1)];
        assert_eq!(unseen_timeline_entries(&initial, &mut seen), initial);
        assert!(unseen_timeline_entries(&initial, &mut seen).is_empty());
        let changed = vec![
            entry("timeline-entry/b", 2, 1),
            entry("timeline-entry/a", 1, 2),
        ];
        assert_eq!(
            unseen_timeline_entries(&changed, &mut seen),
            vec![changed[1].clone(), changed[0].clone()]
        );
        assert!(unseen_timeline_entries(&changed, &mut seen).is_empty());
    }

    #[test]
    fn every_cli_command_has_a_valid_help_surface() {
        fn visit(command: &clap::Command, path: &[String]) {
            if !command.is_hide_set() {
                assert!(
                    command.get_about().is_some(),
                    "{} has no human job or reason",
                    path.join(" ")
                );
            }
            let mut help = command.clone();
            assert!(
                !help.render_long_help().to_string().trim().is_empty(),
                "{} has empty help",
                path.join(" ")
            );
            let subcommands = command.get_subcommands().cloned().collect::<Vec<_>>();
            for subcommand in subcommands {
                let mut child_path = path.to_vec();
                child_path.push(subcommand.get_name().to_owned());
                let mut argv = child_path.clone();
                argv.push("--help".into());
                let error = match Cli::try_parse_from(argv) {
                    Ok(_) => panic!("each command path must accept --help"),
                    Err(error) => error,
                };
                assert_eq!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp,
                    "{} did not render help",
                    child_path.join(" ")
                );
                visit(&subcommand, &child_path);
            }
        }

        let command = Cli::command();
        command.clone().debug_assert();
        visit(&command, &["st".into()]);
    }

    #[test]
    fn top_level_surface_exactly_matches_the_pristine_v0_inventory() {
        let contract: Value = serde_json::from_str(include_str!(
            "../../../docs/st3/operational-state/cli-commands.json"
        ))
        .unwrap();
        let expected = contract["canonical_roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        let command = Cli::command();
        let visible = command
            .get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set())
            .map(|subcommand| subcommand.get_name().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(visible, expected);

        let mut help = command.clone();
        let help = help.render_long_help().to_string();
        assert!(help.contains("Usage: st ["), "{help}");
        assert!(
            command
                .find_subcommand("claude-channel")
                .is_some_and(|command| command.is_hide_set()),
            "the expert ST3 channel lifecycle must remain callable but hidden"
        );

        for legacy in [
            "claude",
            "codex",
            "preview",
            "planning",
            "mission",
            "publish",
            "exec",
            "logs",
            "pty",
            "inspect",
            "wait",
            "doc",
            "eval",
            "graph",
            "status",
            "runtime",
            "context",
            "resource",
            "review",
            "message",
            "gate-result",
        ] {
            assert!(
                command.find_subcommand(legacy).is_none(),
                "legacy root `{legacy}` remains public"
            );
        }
    }

    #[test]
    fn removed_legacy_handlers_are_absent_from_cli_and_server_sources() {
        let cli_source = include_str!("main.rs");
        for handler in [
            "run_preview",
            "run_exec",
            "run_logs",
            "run_status",
            "run_runtime",
            "run_context",
            "run_resource",
            "run_review",
            "run_gate_result",
            "run_eval",
            "run_graph",
            "run_quick",
        ] {
            assert!(
                !cli_source.contains(&format!("fn {handler}(")),
                "legacy CLI handler {handler} remains compiled"
            );
        }
        for command_type in [
            "QuickArgs",
            "PublishArgs",
            "ExecArgs",
            "LogsArgs",
            "StatusArgs",
            "RuntimeCommand",
            "ContextCommand",
            "ResourceCommand",
            "ReviewCommand",
            "GateResultArgs",
            "EvalArgs",
            "GraphArgs",
        ] {
            assert!(
                !cli_source.contains(&format!("struct {command_type}"))
                    && !cli_source.contains(&format!("enum {command_type}")),
                "legacy CLI type {command_type} remains compiled"
            );
        }

        let server_source = include_str!("api.rs");
        for handler in [
            "watch_resource",
            "unwatch_resource",
            "refresh_resource",
            "reset_runtime",
            "quick_claude",
            "quick_codex",
            "start_mission_run",
        ] {
            assert!(
                !server_source.contains(&format!("fn {handler}(")),
                "legacy server handler {handler} remains compiled"
            );
        }
    }

    fn fixture_product_page(kinds: &[&str], has_more: bool) -> ClientPage {
        let resources: Vec<Value> = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/fixtures/resources.json"
        ))
        .unwrap();
        let items = resources
            .into_iter()
            .filter(|resource| {
                resource["kind"]
                    .as_str()
                    .is_some_and(|kind| kinds.contains(&kind))
            })
            .map(|resource| serde_json::from_value(resource).unwrap())
            .collect();
        ClientPage {
            kind: "resource-page".into(),
            collection: "fixture".into(),
            filters: BTreeMap::new(),
            items,
            page: st3_client::PageInfo {
                limit: 100,
                has_more,
                next_cursor: has_more.then(|| "cursor/next".into()),
                cursor_expires_at: None,
            },
            sync: None,
        }
    }

    #[test]
    fn replication_status_says_which_side_holds_what_and_how_long_catching_up_takes() {
        let now = 1_000_000;
        let peer =
            |name: &str, sync: Option<st3::model::ReplicationPeerSync>| ReplicationPeerStatus {
                peer: name.into(),
                status: "up".into(),
                last_success_at_unix_ms: Some(now - 2_000),
                last_error: None,
                schema_digest: None,
                authority_digest: None,
                graph_digest: None,
                sync,
            };
        let output = render_replication_peers(
            &[
                peer(
                    "Silber",
                    Some(st3::model::ReplicationPeerSync {
                        peer_only_envelopes: 124_384,
                        local_only_envelopes: 3,
                        measured_at_unix_ms: now - 2_000,
                        receive_rate_per_second: Some(142.5),
                        catch_up_rate_per_second: Some(140.0),
                        estimated_catch_up_seconds: Some(889),
                        catching_up: true,
                    }),
                ),
                peer(
                    "Quiet",
                    Some(st3::model::ReplicationPeerSync {
                        measured_at_unix_ms: now,
                        ..Default::default()
                    }),
                ),
                peer("Fresh", None),
            ],
            now,
        );
        assert_eq!(
            output,
            "sync\tcatching up: Silber has 124,384 envelopes this node lacks, \
             caught up in about 15m\n\
             peer\tSilber\tup\t\n\
             \x20 last exchange 2s ago\n\
             \x20 Silber has 124,384 envelopes this node lacks\n\
             \x20 this node has 3 envelopes Silber lacks\n\
             \x20 receiving 142.5 envelopes/s, caught up in about 15m (measured 2s ago)\n\
             peer\tQuiet\tup\t\n\
             \x20 last exchange 2s ago\n\
             \x20 in sync: neither side has an envelope the other lacks (measured now)\n\
             peer\tFresh\tup\t\n\
             \x20 last exchange 2s ago\n\
             \x20 difference not measured yet\n"
        );
    }

    #[test]
    fn a_catching_up_page_leads_with_how_far_behind_this_host_is() {
        let mut page = fixture_product_page(&["attention"], false);
        page.sync = Some(st3_client::SyncNotice {
            state: "catching-up".into(),
            peers: vec![st3_client::SyncPeer {
                host_id: "host/Silber".into(),
                peer_only_envelopes: 1,
                local_only_envelopes: 0,
                last_exchange_at: Some("1970-01-01T00:16:38Z".into()),
                estimated_catch_up_seconds: None,
            }],
        });
        let output = render_now_page(&page, "st3 now --as person/nathan");
        assert!(
            output.starts_with(
                "SYNCING  Silber has 1 envelope this host lacks · estimating time to catch up · \
                 last exchange "
            ),
            "{output}"
        );
        assert!(
            output.contains("items below can be out of date"),
            "{output}"
        );
        assert_eq!(
            render_sync_notice(page.sync.as_ref().unwrap(), 1_000_000),
            "SYNCING  Silber has 1 envelope this host lacks · estimating time to catch up · \
             last exchange 2s ago\n  Until then, items below can be out of date. \
             Progress: st3 replication status\n\n"
        );
        assert_eq!(catch_up_estimate(Some(0)), "caught up");
        assert_eq!(catch_up_estimate(Some(59)), "caught up in under a minute");
        assert_eq!(catch_up_estimate(Some(3_601)), "caught up in about 1h 1m");
        assert_eq!(catch_up_estimate(Some(90_000)), "caught up in about 1d 1h");
        assert_eq!(envelope_count(1_234_567), "1,234,567 envelopes");

        page.sync = None;
        assert!(!render_now_page(&page, "st3 now").contains("SYNCING"));
    }

    #[test]
    fn mission_list_counts_active_and_finished_runs_apart() {
        let mut page = fixture_product_page(&["mission"], false);
        let ClientResource::Mission(mission) = &mut page.items[0] else {
            panic!("expected mission fixture");
        };
        mission.runs = (1..=6).map(|run| format!("mission-run/r{run}")).collect();
        let render = |active: Option<usize>, runs: usize| {
            let mut mission = mission.clone();
            mission.runs.truncate(runs);
            mission.active_runs = active;
            render_mission_runs(&mission)
        };
        assert_eq!(render(Some(1), 6), "1 active · 5 finished");
        assert_eq!(render(Some(2), 2), "2 active runs");
        assert_eq!(render(Some(0), 1), "1 finished run");
        assert_eq!(render(Some(0), 0), "0 runs");
        assert_eq!(render(None, 6), "6 runs");
    }

    #[test]
    fn terminal_list_shows_a_working_peek_target() {
        let mut page = fixture_product_page(&["runtime"], false);
        if let ClientResource::Runtime(runtime) = &mut page.items[0] {
            runtime.terminal_id = Some("terminal/agent/release".into());
        } else {
            panic!("expected runtime fixture");
        }
        let rendered = render_product_page("TERMINALS", &page, "st terminals");
        assert!(
            rendered.contains("peek: st terminals peek agent/release"),
            "{rendered}"
        );
    }

    #[test]
    fn work_detail_labels_a_reason_blocked_only_for_blocked_work() {
        let page = fixture_product_page(&["work"], false);
        let ClientResource::Work(work) = &page.items[0] else {
            panic!("expected work fixture");
        };
        let mut work = work.clone();
        work.blocked_reason = Some("the step's lease expired".into());
        work.state = "claimed".into();
        let claimed = render_client_work_detail(&work);
        assert!(!claimed.contains("Blocked:"), "{claimed}");
        assert!(
            claimed.contains("\nReason: the step's lease expired\n"),
            "{claimed}"
        );
        work.state = "blocked".into();
        let blocked = render_client_work_detail(&work);
        assert!(
            blocked.contains("\nBlocked: the step's lease expired\n"),
            "{blocked}"
        );
    }

    #[test]
    fn attention_from_a_retired_requester_says_who_can_close_it() {
        let mut page = fixture_product_page(&["attention"], false);
        let before = render_product_page("NOW", &page, "st now");
        assert!(!before.contains("requester retired"), "{before}");
        let ClientResource::Attention(attention) = &mut page.items[0] else {
            panic!("expected attention fixture");
        };
        attention.header.operational = Some(st3_client::Operational {
            layer: "current".into(),
            actionable: true,
            reasons: vec!["requester-retired".into()],
            owner_generation: None,
            runtime_incarnation: None,
        });
        let rendered = render_product_page("NOW", &page, "st now");
        assert!(
            rendered.contains("  requester retired: only person/nathan can close it\n"),
            "{rendered}"
        );
    }

    #[test]
    fn document_continuation_preserves_prefix_and_history() {
        assert_eq!(
            document_continuation_command(Some("doc/type case"), true, 1, "document/abc"),
            "st documents ls 'doc/type case' --limit 1 --all --cursor document/abc"
        );
    }

    #[test]
    fn product_renderers_have_exact_empty_and_mixed_now_output() {
        assert_eq!(
            render_product_page("NOW", &fixture_product_page(&[], false), "st now"),
            "NOW  0\nNo current items.\n"
        );
        assert_eq!(
            render_product_page(
                "NOW",
                &fixture_product_page(&["attention", "work", "operation"], false),
                "st now"
            ),
            concat!(
                "NOW  3\n",
                "attention/release-review  attention  high  open  Review release\n",
                "  action: st attention show launch/release --as person/nathan\n",
                "work/release/1/build  work  claimed  build  attempt 1\n",
                "  assigned: agent/release\n",
                "  action: st work show work/release/1/build\n",
                "operation/transport-host-b  operation  warning  degraded  Peer is retrying\n",
                "  recovery: st doctor\n",
            )
        );
    }

    #[test]
    fn now_page_without_work_does_not_claim_zero_working() {
        let attention_only = render_now_page(
            &fixture_product_page(&["attention"], false),
            "st now --as person/nathan",
        );
        assert!(
            attention_only.starts_with("NEEDS YOU  1\n"),
            "{attention_only}"
        );
        assert!(!attention_only.contains("WORKING"), "{attention_only}");
        assert!(!attention_only.contains("UNHEALTHY"), "{attention_only}");
        assert!(
            attention_only.ends_with("\nWork: st work ls · Health: st doctor\n"),
            "{attention_only}"
        );

        let with_work = render_now_page(
            &fixture_product_page(&["attention", "work"], false),
            "st now --as person/nathan --owner-run mission-run/release/1",
        );
        assert!(with_work.contains("\nWORKING  1\n"), "{with_work}");
        assert!(!with_work.contains("UNHEALTHY"), "{with_work}");
        assert!(!with_work.contains("Work: st work ls"), "{with_work}");
    }

    #[test]
    fn provider_capacity_backoff_is_bounded_deterministic_and_increases() {
        let first = provider_capacity_backoff_ms("agent/node.worker", "one", 1);
        let second = provider_capacity_backoff_ms("agent/node.worker", "one", 2);
        assert_eq!(
            first,
            provider_capacity_backoff_ms("agent/node.worker", "one", 1)
        );
        assert!(first >= PROVIDER_CAPACITY_BASE_BACKOFF_MS);
        assert!(second > first);
        assert!(
            provider_capacity_backoff_ms("agent/node.worker", "one", u32::MAX)
                <= PROVIDER_CAPACITY_MAX_BACKOFF_MS
                    + PROVIDER_CAPACITY_MAX_BACKOFF_MS.saturating_div(4)
        );
    }

    #[test]
    fn context_occupancy_alone_is_not_reported_as_zero_tokens() {
        let mut page = fixture_product_page(&["session"], false);
        let ClientResource::Session(session) = &mut page.items[0] else {
            panic!("expected session fixture");
        };
        session.usage = Some(
            serde_json::from_value(json!({
                "total_tokens": 0,
                "input_tokens": 0,
                "output_tokens": 0,
                "cached_tokens": 0,
                "incarnation_count": 0,
                "aggregation": "cumulative-per-incarnation-else-response-deltas",
                "context": { "used_tokens": 319465, "observed_at_unix_ms": 1 }
            }))
            .unwrap(),
        );
        let rendered = render_product_page("SESSIONS", &page, "st conversations sessions");
        assert!(!rendered.contains("0 tokens"), "{rendered}");
        assert!(
            rendered.contains("  usage not reported · context 319465 tokens\n"),
            "{rendered}"
        );

        let ClientResource::Session(session) = &mut page.items[0] else {
            unreachable!();
        };
        let usage = session.usage.as_mut().unwrap();
        usage.incarnation_count = 1;
        usage.total_tokens = 1200;
        let rendered = render_product_page("SESSIONS", &page, "st conversations sessions");
        assert!(rendered.contains("  usage 1200 tokens\n"), "{rendered}");
    }

    #[test]
    fn an_empty_mailbox_prints_a_heading_and_no_current_items() {
        assert_eq!(
            render_mailbox("person/nathan", None, false, &[]),
            "MESSAGES  0\nFILTERS  mailbox=person/nathan\nNo current items.\n"
        );
        assert_eq!(
            render_mailbox(
                "agent/worker",
                Some("person/nathan"),
                true,
                &["message/one\tread\tperson/nathan\tHello".into()]
            ),
            concat!(
                "MESSAGES  1\n",
                "FILTERS  mailbox=agent/worker · from=person/nathan · archived=included\n",
                "message/one\tread\tperson/nathan\tHello\n",
            )
        );
    }

    #[test]
    fn claimed_work_renews_for_the_exact_live_harness_incarnation_even_while_idle() {
        let step: StepRunView = serde_json::from_value(json!({
            "subject": "step-run/run/work",
            "run": "mission-run/run",
            "generation": "run-generation/run",
            "step": "work",
            "definition_hash": "definition",
            "status": "claimed",
            "attempt": 1,
            "assigned_to": "agent/node.worker",
            "agentless": false,
            "title": null,
            "worker_reported": false,
            "claimant": "agent/node.worker",
            "claim_incarnation": "worker-one",
            "claim_expires_at_unix_ms": 10,
            "execution_elapsed_ms": 0,
            "readiness_epoch": 1,
            "blocked_reason": null,
            "not_before_unix_ms": null,
            "created_at_unix_ms": 1,
            "updated_at_unix_ms": 1
        }))
        .unwrap();
        let harness: CurrentHarnessView = serde_json::from_value(json!({
            "state": "working",
            "incarnation_id": "worker-one",
            "claim": "claim/harness",
            "observed_at_unix_ms": 1
        }))
        .unwrap();
        assert!(work_claim_has_active_harness(
            &step,
            "agent/node.worker",
            Some(&harness)
        ));

        let mut idle = harness.clone();
        idle.state = "idle".into();
        assert!(work_claim_has_active_harness(
            &step,
            "agent/node.worker",
            Some(&idle)
        ));
        let mut blocked = idle;
        blocked.state = "blocked".into();
        assert!(work_claim_has_active_harness(
            &step,
            "agent/node.worker",
            Some(&blocked)
        ));
        let mut replacement = harness;
        replacement.incarnation_id = "worker-two".into();
        assert!(!work_claim_has_active_harness(
            &step,
            "agent/node.worker",
            Some(&replacement)
        ));
    }

    #[test]
    fn attention_request_help_says_which_targets_end_an_item() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("attention")
            .unwrap()
            .find_subcommand_mut("request")
            .unwrap()
            .render_long_help()
            .to_string();
        for expected in [
            "st attention withdraw",
            "`step-run/` or `run-generation/` target is no longer current",
            "a `mission/` retired or\n  cancelled",
            "`resource/` and `doc/` targets are context",
            "had already ended when you made the request, keeps it open",
        ] {
            assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
        }
    }

    #[test]
    fn machines_help_and_contract_include_configured_failures_by_default() {
        let command = Cli::command();
        let machines = command.find_subcommand("machines").unwrap();
        let all = machines
            .get_arguments()
            .find(|argument| argument.get_id() == "all")
            .unwrap();
        assert_eq!(
            all.get_help().unwrap().to_string(),
            "Include historical and discovered hosts beyond the current configured fleet"
        );

        let contract: Value = serde_json::from_str(include_str!(
            "../../../docs/st3/operational-state/cli-commands.json"
        ))
        .unwrap();
        assert_eq!(
            contract["purposes"]["machines"]["defaults"],
            "current configured fleet including failures"
        );
        assert_eq!(
            contract["purposes"]["attention"]["human_example"],
            "st attention ls --as person/nathan"
        );
        assert_eq!(
            contract["purposes"]["attention"]["json_example"],
            "st attention ls --as person/nathan --json"
        );
    }

    #[test]
    fn machine_and_device_renderers_have_exact_operational_output() {
        assert_eq!(
            render_product_page(
                "MACHINES",
                &fixture_product_page(&["machine"], true),
                "st machines"
            ),
            concat!(
                "MACHINES  1\n",
                "machine/host-a  local\n",
                "  capacity not reported\n",
                "  runtimes 1 running · 1 known\n",
                "  assigned work 1\n",
                "  transport unix local · last success 2026-09-20T11:09:10Z\n",
                "  inspect: st subject show host/host-a\n",
                "More items are available: st machines --cursor cursor/next --limit 100\n",
            )
        );
        assert_eq!(
            render_product_page(
                "DEVICES",
                &fixture_product_page(&["device"], false),
                "st devices --as person/nathan"
            ),
            concat!(
                "DEVICES  1\n",
                "device/ios-release  active  person/nathan/session/ios-release  scopes 4\n",
                "  action: st devices --as person/nathan revoke device/ios-release\n",
            )
        );
    }

    #[test]
    fn activity_renderer_uses_exact_contract_labels_and_cursors() {
        let events: ClientEnvelope<ClientEventPage> = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/fixtures/events.json"
        ))
        .unwrap();
        assert_eq!(
            render_activity_page(&events.value),
            concat!(
                "ACTIVITY  2\n",
                "work/release/1/build  upsert  2026-09-20T12:00:01Z  cursor event-cursor/epoch-a/92\n",
                "session/release-agent/9  timeline.delta  2026-09-20T12:00:02Z  cursor event-cursor/epoch-a/93\n",
            )
        );
        assert_eq!(
            render_activity_page(&ClientEventPage {
                kind: "event-page".into(),
                oldest_cursor: "event-cursor/epoch-a/40".into(),
                resume_cursor: "event-cursor/epoch-a/93".into(),
                items: Vec::new(),
                has_more: true,
            }),
            concat!(
                "ACTIVITY  0\n",
                "No material changes. Resume after event-cursor/epoch-a/93.\n",
                "More changes are available after event-cursor/epoch-a/93.\n",
            )
        );
    }

    #[test]
    fn pty_attach_accepts_a_graph_subject() {
        let cli = Cli::try_parse_from([
            "st3",
            "terminals",
            "attach",
            "agent/fleet/app-web/standing/app-web",
        ])
        .unwrap();
        let Command::Terminals {
            command: PtyCommand::Attach(args),
        } = cli.command
        else {
            panic!("the PTY attach command did not parse");
        };
        assert_eq!(args.subject, "agent/fleet/app-web/standing/app-web");
        assert!(!args.force);
    }

    #[test]
    fn terminal_history_is_explicit() {
        let cli = Cli::try_parse_from(["st3", "terminals", "ls", "--all"]).unwrap();
        let Command::Terminals {
            command: PtyCommand::Ls { all, .. },
        } = cli.command
        else {
            panic!("the terminal list command did not parse");
        };
        assert!(all);
    }

    #[test]
    fn terminal_screen_accepts_a_remote_owner_subject_and_person() {
        let cli = Cli::try_parse_from([
            "st3",
            "--json",
            "terminals",
            "screen",
            "terminal/agent/fleet/app-web/standing/app-web",
            "--as",
            "person/nathan",
        ])
        .unwrap();
        assert!(cli.json);
        let Command::Terminals {
            command: PtyCommand::Screen(args),
        } = cli.command
        else {
            panic!("the terminal screen command did not parse");
        };
        assert_eq!(
            args.subject,
            "terminal/agent/fleet/app-web/standing/app-web"
        );
        assert_eq!(args.person.as_deref(), Some("person/nathan"));
    }

    #[test]
    fn terminal_screen_renderer_preserves_each_terminal_row() {
        let response: ClientEnvelope<ClientTerminalScreen> = serde_json::from_str(include_str!(
            "../../../docs/st3/client-v0/fixtures/terminal-screen.json"
        ))
        .unwrap();
        assert_eq!(
            render_terminal_screen(&response.value),
            "$ cargo build\nFinished\n$\n"
        );
    }

    #[test]
    fn client_terminal_lifecycle_commands_parse_without_changing_local_attach() {
        let attach = Cli::try_parse_from([
            "st3",
            "terminals",
            "attach-info",
            "terminal/agent/fleet/app-web/standing/app-web",
        ])
        .unwrap();
        assert!(matches!(
            attach.command,
            Command::Terminals {
                command: PtyCommand::AttachInfo(_)
            }
        ));

        let stream = Cli::try_parse_from([
            "st3",
            "terminals",
            "stream",
            "terminal/agent/fleet/app-web/standing/app-web",
            "--capability",
            "test-capability",
            "--incarnation",
            "runtime-1",
            "--count",
            "3",
        ])
        .unwrap();
        let Command::Terminals {
            command: PtyCommand::Stream(args),
        } = stream.command
        else {
            panic!("stream did not parse");
        };
        assert_eq!(args.incarnation.as_deref(), Some("runtime-1"));
        assert_eq!(args.count, Some(3));

        let input = Cli::try_parse_from([
            "st3",
            "terminals",
            "input-client",
            "terminal/agent/fleet/app-web/standing/app-web",
            "hello",
            "--key",
        ])
        .unwrap();
        assert!(matches!(
            input.command,
            Command::Terminals {
                command: PtyCommand::InputClient(PtyClientInputArgs { key: true, .. })
            }
        ));

        let detach = Cli::try_parse_from([
            "st3",
            "terminals",
            "detach-client",
            "terminal-attachment/example",
            "--incarnation",
            "runtime/example:1",
        ])
        .unwrap();
        assert!(matches!(
            detach.command,
            Command::Terminals {
                command: PtyCommand::DetachClient(_)
            }
        ));
    }

    #[test]
    fn harness_diagnostic_requires_and_derives_the_agent_identity() {
        let cli = Cli::try_parse_from([
            "st3",
            "diagnostic",
            "--as",
            "agent/run/worker",
            "--code",
            "driver-failed",
            "--reason",
            "the native driver exited",
            "--incarnation",
            "runtime:one",
        ])
        .unwrap();
        let Command::Diagnostic(args) = cli.command else {
            panic!("the harness diagnostic command did not parse");
        };
        assert_eq!(args.actor, "agent/run/worker");
        assert_eq!(args.severity, "error");
        assert_eq!(args.incarnation.as_deref(), Some("runtime:one"));
    }

    #[test]
    fn every_extension_harness_has_a_hidden_native_channel_driver() {
        for driver in ["pi-channel", "omp-channel"] {
            let cli =
                Cli::try_parse_from(["st3", "driver", driver, "--subject", "agent/run/worker"])
                    .unwrap_or_else(|error| panic!("{driver} did not parse: {error}"));
            let Command::Driver(args) = cli.command else {
                panic!("{driver} did not select the hidden driver command");
            };
            assert_eq!(args.driver, driver);
            assert_eq!(args.subject.as_deref(), Some("agent/run/worker"));
        }
    }

    #[test]
    fn pty_attach_accepts_an_explicit_nested_override() {
        let cli = Cli::try_parse_from([
            "st3",
            "terminals",
            "attach",
            "agent/fleet/app-web/standing/app-web",
            "--force",
        ])
        .unwrap();
        let Command::Terminals {
            command: PtyCommand::Attach(args),
        } = cli.command
        else {
            panic!("the PTY attach command did not parse");
        };
        assert!(args.force);
    }

    #[test]
    fn an_agent_wait_stops_for_messages_work_and_an_empty_lease() {
        let actor = "agent/worker";
        assert_eq!(
            wait_interruption_reason(actor, true, &[], &["message/new".into()]),
            Some(
                "the wait stopped because agent/worker has a new message: message/new. Run `st conversations ls`"
                    .into()
            )
        );
        assert_eq!(
            wait_interruption_reason(actor, true, &["step-run/new".into()], &[]),
            Some(
                "the wait stopped because agent/worker has ready work: step-run/new. Run `st work ls`"
                    .into()
            )
        );
        assert_eq!(
            wait_interruption_reason(
                actor,
                true,
                &["step-run/new".into()],
                &["message/new".into()]
            ),
            Some(
                "the wait stopped because agent/worker has ready work: step-run/new. Run `st work ls`"
                    .into()
            )
        );
        assert!(
            wait_interruption_reason(actor, false, &[], &[])
                .unwrap()
                .contains("cannot wait without claimed work")
        );
        assert_eq!(wait_interruption_reason(actor, true, &[], &[]), None);
    }

    #[test]
    fn a_short_message_party_resolves_to_its_current_mission_run() {
        assert_eq!(
            normalize_message_subject_in_run("worker", Some("run-id")),
            "agent/run-id/worker"
        );
        assert_eq!(
            normalize_message_subject_in_run("agent/global.worker", Some("run-id")),
            "agent/global.worker"
        );
        assert_eq!(
            normalize_message_subject_in_run("worker", None),
            "agent/worker"
        );
    }

    #[test]
    fn mission_start_help_shows_the_run_subject_an_id_names() {
        use clap::CommandFactory as _;
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("missions")
            .and_then(|missions| missions.find_subcommand_mut("start"))
            .expect("missions start")
            .render_help()
            .to_string();
        assert!(help.contains("`mission-run/release/demo/1`"), "{help}");
        assert!(help.contains("`mission-run/1`"), "{help}");
    }

    #[test]
    fn mission_start_accepts_an_explicit_run_id() {
        let cli = Cli::try_parse_from([
            "st3",
            "missions",
            "start",
            "release/demo",
            "--id",
            "release/demo/test",
            "--as",
            "agent/operator",
        ])
        .unwrap();
        let Command::Missions {
            command: MissionViewCommand::Start(args),
        } = cli.command
        else {
            panic!("the mission start command did not parse");
        };
        assert_eq!(args.mission, "release/demo");
        assert_eq!(args.id.as_deref(), Some("release/demo/test"));
        assert_eq!(args.actor, "agent/operator");
    }

    #[test]
    fn mission_start_after_names_the_run_to_wait_for() {
        let cli = Cli::try_parse_from([
            "st3",
            "missions",
            "start",
            "release/demo",
            "--after",
            "release/build/1",
            "--as",
            "person/operator",
        ])
        .unwrap();
        let Command::Missions {
            command: MissionViewCommand::Start(args),
        } = cli.command
        else {
            panic!("the mission start command did not parse");
        };
        assert_eq!(args.after.as_deref(), Some("release/build/1"));

        let kdl = mission_run_intent(
            "release/demo/2",
            "release/demo",
            &"a".repeat(64),
            Path::new("/work/demo"),
            "person/operator",
            &BTreeMap::new(),
            "run",
            Some("mission-run/release/build/1"),
        );
        assert!(
            kdl.contains("after \"mission-run/release/build/1\""),
            "{kdl}"
        );
        let intent = st3::graph::parse_intent(&kdl, "node").unwrap();
        let creation = intent.mission_runs["mission-run/release/demo/2"]
            .creation
            .as_ref()
            .unwrap();
        assert_eq!(
            creation.after.as_deref(),
            Some("mission-run/release/build/1")
        );
    }

    #[test]
    fn mission_publish_requires_an_explicit_person_or_agent_actor() {
        let cli = Cli::try_parse_from([
            "st3",
            "missions",
            "publish",
            "missions/typecase.kdl",
            "--as",
            "agent/fleet/cos/standing/cos",
        ])
        .unwrap();
        let Command::Missions {
            command: MissionViewCommand::Publish(args),
        } = cli.command
        else {
            panic!("the mission publish command did not parse");
        };
        assert_eq!(args.file, PathBuf::from("missions/typecase.kdl"));
        assert_eq!(args.actor, "agent/fleet/cos/standing/cos");
        assert!(
            Cli::try_parse_from([
                "st3",
                "missions",
                "publish",
                "missions/typecase.kdl",
                "--as",
                "cos",
            ])
            .is_err()
        );
    }

    #[test]
    fn mission_cancel_requires_an_exact_actor_and_reason() {
        let cli = Cli::try_parse_from([
            "st3",
            "missions",
            "cancel",
            "mission-run/release/demo",
            "--reason",
            "the run was superseded",
            "--as",
            "person/nathan",
        ])
        .unwrap();
        let Command::Missions {
            command: MissionViewCommand::Cancel(args),
        } = cli.command
        else {
            panic!("the mission cancel command did not parse");
        };
        assert_eq!(args.mission_run, "mission-run/release/demo");
        assert_eq!(args.reason, "the run was superseded");
        assert_eq!(args.actor, "person/nathan");
    }

    #[test]
    fn mission_show_accepts_follow() {
        let cli = Cli::try_parse_from([
            "st3",
            "missions",
            "show",
            "mission-run/release/demo",
            "--follow",
        ])
        .unwrap();
        let Command::Missions {
            command: MissionViewCommand::Show(args),
        } = cli.command
        else {
            panic!("the mission show command did not parse");
        };
        assert_eq!(args.mission_or_run, "mission-run/release/demo");
        assert!(args.follow);
    }

    #[test]
    fn cli_exposes_launch_without_removed_planning_aliases() {
        assert!(Cli::try_parse_from(["st3", "plan", "show", "example"]).is_err());
        assert!(Cli::try_parse_from(["st3", "planning", "show", "example"]).is_err());
        let cli = Cli::try_parse_from(["st3", "launch", "show", "example"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Launch {
                command: LaunchCommand::Show(_)
            }
        ));
    }

    #[test]
    fn wait_timeout_accepts_bounded_units_and_zero() {
        assert_eq!(parse_timeout("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_timeout("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_timeout("0").unwrap(), Duration::ZERO);
        assert!(parse_timeout("forever").is_err());
    }

    #[test]
    fn wait_reads_resource_status_from_observed_facts() {
        let resource = json!({
            "baseline": true,
            "facts": {"status": "ready"},
            "kind": "filesystem.file"
        });
        assert_eq!(
            st3::model::projected_actual_status(Some(&resource)),
            Some("ready")
        );

        let runtime = json!({"fields": {"status": "running"}});
        assert_eq!(
            st3::model::projected_actual_status(Some(&runtime)),
            Some("running")
        );
    }

    #[test]
    fn wait_accepts_message_delivery() {
        validate_wait_condition("delivered").unwrap();
        let message = serde_json::json!({ "status": "delivered" });
        assert_eq!(
            st3::model::projected_actual_status(Some(&message)),
            Some("delivered")
        );
    }

    #[test]
    fn wait_accepts_a_standing_mission_run() {
        validate_wait_condition("standing").unwrap();
        let run = serde_json::json!({ "status": "standing" });
        assert_eq!(
            st3::model::projected_actual_status(Some(&run)),
            Some("standing")
        );
    }

    #[test]
    fn native_driver_binds_the_local_pty_incarnation_instead_of_a_stale_graph_value() {
        let observations = vec![st_runtime::PtyObservation {
            name: "run.worker".into(),
            status: "running".into(),
            exit_code: None,
            pid: Some(42),
            created_at: Some("2026-09-22T12:00:00.000Z".into()),
            display_name: None,
            tags: BTreeMap::from([("st3.subject".into(), "agent/run/worker".into())]),
        }];

        assert_eq!(
            pty_observation_incarnation("agent/run/worker", &observations).as_deref(),
            Some("42:2026-09-22T12:00:00.000Z")
        );
        assert_eq!(
            pty_observation_incarnation("agent/run/other", &observations),
            None
        );
    }

    #[test]
    fn an_ended_harness_uses_the_registered_state() {
        assert_eq!(
            harness_activity_state(st2::harness_state::Activity::Ended),
            "ended"
        );
        st3_schema::registry()
            .validate_claim(
                "agent/run/worker",
                "harness.observed",
                &BTreeMap::from([("state".into(), Value::String("ended".into()))]),
            )
            .unwrap();
    }

    #[test]
    fn typed_claude_accepts_tui_options_and_rejects_headless_protocol_options() {
        reject_noninteractive_claude_argv(&["claude".into(), "--model".into(), "opus".into()])
            .unwrap();
        reject_noninteractive_claude_argv(&[
            "claude".into(),
            "--remote-control".into(),
            "cos".into(),
        ])
        .unwrap();

        for argv in [
            vec!["claude".into(), "-p".into()],
            vec!["claude".into(), "--print".into()],
            vec!["claude".into(), "--input-format=stream-json".into()],
            vec![
                "claude".into(),
                "--output-format".into(),
                "stream-json".into(),
            ],
        ] {
            let error = reject_noninteractive_claude_argv(&argv).unwrap_err();
            assert!(error.to_string().contains("use an `exec` declaration"));
        }
    }

    #[test]
    fn claude_delivery_requires_the_exact_incarnations_prompt_submit_receipt() {
        let root = tempfile::tempdir().unwrap();
        let mut writer = st2::harness_timeline::Writer::new(root.path(), "claude", "inc-2");
        writer
            .append(
                "prompt-1",
                st2::harness_timeline::Role::User,
                st2::harness_timeline::EntryType::Content,
                serde_json::json!({
                    "text": "[st3-delivery:1784649988123-abc23z.md]\nhello"
                }),
                true,
            )
            .unwrap();

        assert!(
            claude_channel_consumed_delivery_filenames(root.path(), "inc-1")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            claude_channel_consumed_delivery_filenames(root.path(), "inc-2").unwrap(),
            BTreeSet::from(["1784649988123-abc23z.md".to_owned()])
        );
    }

    #[test]
    fn native_exit_claims_are_unique_per_incarnation() {
        assert_eq!(
            native_exit_key("agent/node.worker", "node.worker", "one"),
            native_exit_key("agent/node.worker", "node.worker", "one"),
        );
        assert_ne!(
            native_exit_key("agent/node.worker", "node.worker", "one"),
            native_exit_key("agent/node.worker", "node.worker", "two"),
        );
    }

    #[test]
    fn claude_receipts_use_provider_session_not_runtime_incarnation() {
        let root = tempfile::tempdir().unwrap();
        let mut writer =
            st2::harness_timeline::Writer::new(root.path(), "claude", "provider-current");
        writer
            .append(
                "prompt-1",
                st2::harness_timeline::Role::User,
                st2::harness_timeline::EntryType::Content,
                serde_json::json!({"text": "[st3-delivery:1784649988123-abc23z.md] hello"}),
                true,
            )
            .unwrap();
        let receipt_incarnation =
            claude_receipt_incarnation("runtime-current", Some("provider-current"));
        assert_eq!(
            claude_channel_consumed_delivery_filenames(root.path(), receipt_incarnation).unwrap(),
            BTreeSet::from(["1784649988123-abc23z.md".to_owned()]),
        );
    }

    #[test]
    fn successor_never_adopts_the_predecessors_terminal_harness_record() {
        let predecessor = br#"{"state":"ended","exit":"exit 0"}"#;
        assert!(!harness_record_belongs_to_current_session(
            false,
            Some(predecessor),
            Some(predecessor),
        ));
        assert!(!harness_record_belongs_to_current_session(
            false,
            Some(predecessor),
            None,
        ));

        let claim = br#"{"state":"ended","reason":"superseded","incarnation":"next"}"#;
        assert!(harness_record_belongs_to_current_session(
            false,
            Some(predecessor),
            Some(claim),
        ));
        assert!(
            harness_record_belongs_to_current_session(true, Some(predecessor), Some(predecessor)),
            "once the successor fenced ownership, predecessor bytes cannot regain ownership"
        );
    }

    #[test]
    fn extension_channel_state_outlives_its_st2_launch_placeholder() {
        for driver in ["pi", "omp"] {
            assert!(
                !native_file_may_override_channel(driver),
                "{driver} reports harness state through the ST3 extension channel"
            );
        }
        for driver in ["claude", "opencode"] {
            assert!(native_file_may_override_channel(driver));
        }
    }

    #[test]
    fn message_references_round_trip_nested_ids_and_projected_files() {
        assert_eq!(
            normalize_message_reference("message/kickoff/run-1"),
            "kickoff/run-1"
        );
        assert_eq!(
            normalize_message_reference("kickoff%2Frun-1"),
            "kickoff/run-1"
        );
        assert_eq!(
            normalize_message_reference("/tmp/inbox/00000000000000000008-kickoff%2Frun-1.md"),
            "kickoff/run-1"
        );
    }

    #[test]
    fn message_list_requires_one_exact_mailbox_and_explicit_identity_wins() {
        let bare = Cli::try_parse_from(["st3", "conversations", "ls"]).unwrap();
        let Command::Conversations {
            command: MessageCommand::Ls(bare),
        } = bare.command
        else {
            panic!("conversations ls did not parse");
        };
        assert!(bare.identity.is_none());

        assert_eq!(
            message_list_identity(None, Some("agent/from-environment".into())).unwrap(),
            "agent/from-environment"
        );
        assert_eq!(
            message_list_identity(
                Some("person/explicit".into()),
                Some("agent/from-environment".into())
            )
            .unwrap(),
            "person/explicit"
        );
        let error = message_list_identity(None, Some(String::new())).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("refusing to list every fleet message")
        );

        let explicit =
            Cli::try_parse_from(["st3", "conversations", "ls", "agent/explicit", "--archive"])
                .unwrap();
        let Command::Conversations {
            command: MessageCommand::Ls(explicit),
        } = explicit.command
        else {
            panic!("conversations ls with an identity did not parse");
        };
        assert_eq!(explicit.identity.as_deref(), Some("agent/explicit"));
        assert!(explicit.archive);
    }

    #[test]
    fn message_replies_route_to_the_other_participant() {
        let original = MessageView {
            subject: "message/original".into(),
            from: "agent/h".into(),
            to: "agent/s".into(),
            content: "Hello".into(),
            status: "read".into(),
            title: Some("Greeting".into()),
            in_reply_to: None,
            tags: Vec::new(),
            created_index: 1,
        };

        assert_eq!(
            message_reply_recipient(&original, "agent/h").unwrap(),
            "agent/s"
        );
        assert_eq!(
            message_reply_recipient(&original, "agent/s").unwrap(),
            "agent/h"
        );
        let error = message_reply_recipient(&original, "agent/outsider").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot reply as a non-participant")
        );
    }

    #[test]
    fn message_archive_accepts_more_than_one_reference() {
        let cli = Cli::try_parse_from([
            "st3",
            "conversations",
            "archive",
            "first",
            "second",
            "third",
            "--as",
            "agent/sup",
        ])
        .unwrap();
        let Command::Conversations {
            command: MessageCommand::Archive(args),
        } = cli.command
        else {
            panic!("the archive command did not parse");
        };
        assert_eq!(args.references, ["first", "second", "third"]);
        assert_eq!(args.actor.as_deref(), Some("agent/sup"));
    }

    #[test]
    fn message_read_accepts_multiple_canonical_references() {
        let cli = Cli::try_parse_from([
            "st3",
            "conversations",
            "read",
            "message/first",
            "message/second",
            "--as",
            "agent/sup",
            "--archive",
        ])
        .unwrap();
        let Command::Conversations {
            command: MessageCommand::Read(args),
        } = cli.command
        else {
            panic!("the read command did not parse");
        };
        assert_eq!(args.references, ["message/first", "message/second"]);
        assert_eq!(args.actor.as_deref(), Some("agent/sup"));
        assert!(args.archive);
    }

    #[tokio::test]
    async fn message_read_returns_the_committed_lifecycle_state() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let state = AppState {
            store: Arc::new(Store::open_memory("message-read-state").unwrap()),
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "message-read-state".into(),
            state_dir: root.path().to_path_buf(),
            pty_root: root.path().join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: PlannerSpec::default(),
        };
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            serve_unix(&server_socket, router(state)).await.unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(socket.exists(), "the test API socket did not start");
        let client = Client::unix(&socket);

        let first: MessageView = client
            .post(
                "/v1/messages",
                &MessageSendRequest {
                    idempotency_key: "message-read-final-state".into(),
                    from: "agent/sender".into(),
                    to: "agent/sup".into(),
                    content: "Read me".into(),
                    title: None,
                    in_reply_to: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap();
        let read = read_message_after_lifecycle(&client, &first.subject, "agent/sup", false)
            .await
            .unwrap();
        assert_eq!(read.status, "read");

        let second: MessageView = client
            .post(
                "/v1/messages",
                &MessageSendRequest {
                    idempotency_key: "message-read-archive-final-state".into(),
                    from: "agent/sender".into(),
                    to: "agent/sup".into(),
                    content: "Archive me".into(),
                    title: None,
                    in_reply_to: None,
                    tags: Vec::new(),
                },
            )
            .await
            .unwrap();
        let archived = read_message_after_lifecycle(&client, &second.subject, "agent/sup", true)
            .await
            .unwrap();
        assert_eq!(archived.status, "closed");

        server.abort();
    }

    #[test]
    fn agents_requires_an_explicit_subcommand_and_exposes_seat_lifecycle_commands() {
        assert!(Cli::try_parse_from(["st3", "agents"]).is_err());

        let cli = Cli::try_parse_from(["st3", "agents", "ls", "--status", "running", "--enrich"])
            .unwrap();
        let Command::Agents {
            command: AgentsCommand::Ls(args),
        } = cli.command
        else {
            panic!("agents ls did not parse");
        };
        assert_eq!(args.status.as_deref(), Some("running"));
        assert!(args.enrich);

        let cli = Cli::try_parse_from(["st3", "agents", "tree", "--all"]).unwrap();
        let Command::Agents {
            command: AgentsCommand::Tree(args),
        } = cli.command
        else {
            panic!("agents tree did not parse");
        };
        assert!(args.all);

        let cli = Cli::try_parse_from(["st3", "agents", "show", "worker", "--all"]).unwrap();
        let Command::Agents {
            command: AgentsCommand::Show { subject, all },
        } = cli.command
        else {
            panic!("agents show did not parse");
        };
        assert_eq!(subject, "worker");
        assert!(all);

        let cli = Cli::try_parse_from([
            "st3",
            "agents",
            "start",
            "fleet/cos/standing/cos",
            "--harness",
            "claude",
            "--model",
            "opus",
            "--as",
            "person/nathan",
            "--print-kdl",
        ])
        .unwrap();
        let Command::Agents {
            command: AgentsCommand::Start(args),
        } = cli.command
        else {
            panic!("agents start did not parse");
        };
        let kdl = agent_start_document(&args).unwrap();
        let intent = st3::parse_intent(&kdl, "node").unwrap();
        assert!(intent.subjects.contains_key("agent/fleet/cos/standing/cos"));
        assert!(
            intent.subjects["agent/fleet/cos/standing/cos"]
                .owner_run
                .is_none()
        );

        let cli = Cli::try_parse_from([
            "st3",
            "agents",
            "stop",
            "fleet/cos/standing/cos",
            "--as",
            "person/nathan",
            "--print-kdl",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Agents {
                command: AgentsCommand::Stop(_)
            }
        ));
    }

    #[test]
    fn review_mutations_use_the_explicit_as_actor() {
        let cli = Cli::try_parse_from([
            "st3",
            "attention",
            "approve",
            "attention/item",
            "--as",
            "person/reviewer",
        ])
        .unwrap();
        let Command::Attention {
            command: AttentionCommand::Approve(args),
        } = cli.command
        else {
            panic!("review approve did not parse");
        };
        assert_eq!(args.actor, "person/reviewer");
        assert!(
            Cli::try_parse_from([
                "st3",
                "attention",
                "approve",
                "attention/item",
                "--as",
                "reviewer",
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["st3", "attention", "approve", "attention/item",]).is_err());
    }

    #[test]
    fn human_mutations_reject_missing_or_shorthand_identity() {
        let missing = [
            vec![
                "st3",
                "missions",
                "cancel",
                "mission-run/demo",
                "--reason",
                "done",
            ],
            vec!["st3", "launch", "start", "--id", "demo"],
            vec!["st3", "launch", "approve", "launch/demo", "hash"],
            vec!["st3", "launch", "run", "launch/demo"],
            vec!["st3", "launch", "cancel", "launch/demo"],
            vec!["st3", "import", "run", "session/demo"],
        ];
        for argv in missing {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "accepted human action without --as: {argv:?}"
            );
        }

        for argv in [
            vec!["st3", "attention", "ls", "--as", "nathan"],
            vec!["st3", "devices", "--as", "nathan"],
            vec!["st3", "import", "run", "session/demo", "--as", "nathan"],
        ] {
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "accepted shorthand human identity: {argv:?}"
            );
        }
    }

    #[test]
    fn full_control_device_pairing_is_an_explicit_cli_choice() {
        let limited =
            Cli::try_parse_from(["st3", "devices", "--as", "person/nathan", "pair", "iPhone"])
                .unwrap();
        let Command::Devices(DevicesArgs {
            command: Some(DevicesCommand::Pair { full_control, .. }),
            ..
        }) = limited.command
        else {
            panic!("expected a device pairing command")
        };
        assert!(!full_control);

        let full = Cli::try_parse_from([
            "st3",
            "devices",
            "--as",
            "person/nathan",
            "pair",
            "--full-control",
            "iPhone",
        ])
        .unwrap();
        let Command::Devices(DevicesArgs {
            command: Some(DevicesCommand::Pair { full_control, .. }),
            ..
        }) = full.command
        else {
            panic!("expected a device pairing command")
        };
        assert!(full_control);
    }

    #[test]
    fn legacy_publish_is_not_a_public_alias() {
        assert!(
            Cli::try_parse_from(["st3", "publish", "mission.kdl", "--as", "agent/operator"])
                .is_err()
        );
    }

    #[test]
    fn canonical_intent_helpers_print_current_direct_kdl() {
        let message = message_mission_intent(
            "message/test",
            "test",
            "person/sender",
            "person/recipient",
            "Hello.",
            Some("Greeting"),
            None,
            &["example".into()],
        );
        let message = st3::parse_intent(&message, "node").unwrap();
        assert!(message.missions.contains_key("message/test"));

        let planning = planning_session_intent(
            "planning/example/01990000000070008000000000000000",
            "example",
            &format!("doc/planning/example/request@{}", "a".repeat(64)),
            Path::new("/work/example"),
            "person/operator",
            &PlannerSpec::default(),
            None,
        );
        let planning = st3::parse_intent(&planning, "node").unwrap();
        assert_eq!(planning.planning_sessions.len(), 1);
    }

    #[test]
    fn subscription_request_decisions_need_a_person() {
        let cli = Cli::try_parse_from([
            "st3",
            "missions",
            "release",
            "request-id",
            "--as",
            "person/operator",
            "--reason",
            "the held review is real",
        ])
        .unwrap();
        let Command::Missions {
            command: MissionViewCommand::Release(args),
        } = cli.command
        else {
            panic!("the missions release command did not parse");
        };
        assert_eq!(args.actor, "person/operator");
        assert!(
            Cli::try_parse_from([
                "st3",
                "missions",
                "cancel-request",
                "request-id",
                "--as",
                "agent/node.triage",
                "--reason",
                "an agent cannot decide",
            ])
            .is_err()
        );
    }

    #[test]
    fn work_revise_accepts_print_only_mode() {
        let cli = Cli::try_parse_from([
            "st3",
            "work",
            "revise",
            "mission-run/release",
            "release.kdl",
            "--reason",
            "add a gate",
            "--as",
            "person/operator",
            "--print-kdl",
        ])
        .unwrap();
        let Command::Work {
            command: WorkCommand::Revise(args),
        } = cli.command
        else {
            panic!("the work revise command did not parse");
        };
        assert!(args.print_kdl);
    }

    #[test]
    fn work_revise_counts_only_top_level_missions() {
        let source = r#"
version 2
mission "review" state="ready" {
  goal "Review the source."
  loop "rounds" {
    max-rounds 2
    round {
      completion { when "all-steps-exhausted" }
      step "write" {
        goal "Write the review."
      }
    }
  }
}
"#;
        let parsed = st3::parse_intent(source, "local").unwrap();
        assert!(parsed.missions.len() > 1);
        assert_eq!(
            st3::mission::top_level_mission_ids(&parsed.missions),
            std::collections::BTreeSet::from(["review".to_owned()])
        );
    }

    #[test]
    fn work_wake_is_explicit_and_terminal_ding_is_not_a_driver() {
        let cli = Cli::try_parse_from([
            "st3",
            "work",
            "wake",
            "step-run/example/work",
            "--as",
            "person/operator",
            "--reason",
            "retry native delivery",
        ])
        .unwrap();
        let Command::Work {
            command: WorkCommand::Wake(args),
        } = cli.command
        else {
            panic!("the work wake command did not parse");
        };
        assert_eq!(args.subject, "step-run/example/work");
        assert_eq!(args.actor.as_deref(), Some("person/operator"));
        assert!(Cli::try_parse_from(["st3", "driver", "ding"]).is_err());
    }

    #[test]
    fn review_commands_parse_a_filter_and_an_owner_target() {
        let list =
            Cli::try_parse_from(["st3", "attention", "ls", "--as", "person/nathan"]).unwrap();
        let Command::Attention {
            command: AttentionCommand::Ls { actor, .. },
        } = list.command
        else {
            panic!("the review list command did not parse");
        };
        assert_eq!(actor.as_deref(), Some("person/nathan"));

        let approve = Cli::try_parse_from([
            "st3",
            "attention",
            "approve",
            "mission-run/release/one",
            "--as",
            "person/nathan",
        ])
        .unwrap();
        let Command::Attention {
            command: AttentionCommand::Approve(args),
        } = approve.command
        else {
            panic!("the review approve command did not parse");
        };
        assert_eq!(args.target, "mission-run/release/one");
    }

    #[test]
    fn attention_commands_parse_list_request_and_resolution() {
        let list =
            Cli::try_parse_from(["st3", "attention", "ls", "--as", "person/nathan"]).unwrap();
        let Command::Attention {
            command: AttentionCommand::Ls { actor, .. },
        } = list.command
        else {
            panic!("the attention list command did not parse");
        };
        assert_eq!(actor.as_deref(), Some("person/nathan"));

        let request = Cli::try_parse_from([
            "st3",
            "attention",
            "request",
            "--for",
            "person/nathan",
            "--title",
            "Fabric needs review",
            "--reason",
            "The queue did not recover.",
            "--target",
            "mission-run/fabric",
            "--as",
            "agent/fabric/worker",
            "--idempotency-key",
            "fabric-fault",
        ])
        .unwrap();
        let Command::Attention {
            command: AttentionCommand::Request(args),
        } = request.command
        else {
            panic!("the attention request command did not parse");
        };
        assert_eq!(args.severity, "error");
        assert_eq!(args.targets, ["mission-run/fabric"]);
        assert_eq!(args.until, None);

        let until = Cli::try_parse_from([
            "st3",
            "attention",
            "request",
            "--for",
            "person/nathan",
            "--title",
            "Publish this revision",
            "--reason",
            "Publish the prepared revision as a person.",
            "--target",
            "mission-run/release/one",
            "--until",
            "completed",
            "--as",
            "agent/release/worker",
        ])
        .unwrap();
        let Command::Attention {
            command: AttentionCommand::Request(args),
        } = until.command
        else {
            panic!("the attention request with until did not parse");
        };
        assert_eq!(args.until.as_deref(), Some("completed"));

        let resolve = Cli::try_parse_from([
            "st3",
            "attention",
            "resolve",
            "attention/fabric",
            "--outcome",
            "dismissed",
            "--as",
            "person/nathan",
        ])
        .unwrap();
        let Command::Attention {
            command: AttentionCommand::Resolve(args),
        } = resolve.command
        else {
            panic!("the attention resolve command did not parse");
        };
        assert_eq!(args.subject, "attention/fabric");
        assert_eq!(args.outcome, "dismissed");
    }

    #[test]
    fn a_native_driver_gets_one_graph_message_projection() {
        let root = tempfile::tempdir().unwrap();
        let (catalog, agent_dir, identity, runtime_id) =
            prepare_native_driver_in("agent/node.worker", root.path()).unwrap();
        let discovery = agent_spec::discovery::discover_strict(&catalog);
        assert!(discovery.errors.is_empty(), "{:?}", discovery.errors);
        assert_eq!(discovery.specs.len(), 1);
        assert_eq!(discovery.specs[0].identity, "node.worker");
        assert_eq!(
            discovery.specs[0].path.parent().unwrap(),
            agent_dir.as_path()
        );
        assert_eq!(identity, "node.worker");
        assert_eq!(runtime_id, "node.worker");
        let (long_catalog, _, _, _) =
            prepare_native_driver_in("agent/fleet/app-web/standing/app-web", root.path()).unwrap();
        let report = st2::validate::validate_for_host(&long_catalog, &st2::run::detect_host());
        assert!(report.issues.is_empty(), "{report:?}");
    }

    #[test]
    fn native_timeline_fences_provider_session_but_claims_runtime_session() {
        let record = st2::harness_timeline::Record {
            schema: "st2.harness-timeline.v1".into(),
            driver: "claude".into(),
            incarnation_id: "provider-current".into(),
            next_sequence: 2,
            operations: Vec::new(),
        };
        assert!(timeline_record_is_current(
            &record,
            "claude",
            Some("provider-current")
        ));
        assert!(!timeline_record_is_current(
            &record,
            "claude",
            Some("provider-old")
        ));
        assert!(!timeline_record_is_current(
            &record,
            "codex",
            Some("provider-current")
        ));
        let fields = timeline_claim_fields(
            st2::harness_timeline::Operation {
                operation: "append".into(),
                entry_id: "timeline-entry/test".into(),
                sequence: 1,
                revision: 1,
                role: "assistant".into(),
                entry_type: "content".into(),
                final_entry: true,
                body: json!({"text":"answer"}),
                driver: "claude".into(),
                incarnation_id: "provider-current".into(),
                observed_at_unix_ms: 1,
                source_id: None,
            },
            "runtime-current",
        );
        assert_eq!(fields["incarnation_id"], "runtime-current");
        assert!(fields.get("evidence_incarnation").is_none());
        assert_eq!(fields["body"]["text"], "answer");
    }

    #[test]
    fn an_unread_native_message_is_ready_for_a_delivery_claim() {
        let root = tempfile::tempdir().unwrap();
        let inbox = root.path().join("inbox");
        let archive = root.path().join("archive");
        st2::message::send_to_inbox(
            &inbox,
            "requester",
            Some("Start"),
            None,
            &["st3-message:message/kickoff".into()],
            "Do the work.",
        )
        .unwrap();

        assert_eq!(
            projected_message_subjects(&inbox, &archive).unwrap(),
            BTreeSet::from(["message/kickoff".into()])
        );
    }

    #[test]
    fn graph_delivery_waits_for_the_exact_consumed_native_file() {
        let first = "1786380000000-aaa111.md";
        let second = "1786380000001-bbb222.md";
        let transport_accepted_only = BTreeSet::new();
        assert!(!native_delivery_receipted(&transport_accepted_only, first));

        let consumed = BTreeSet::from([first.to_owned()]);
        assert!(native_delivery_receipted(&consumed, first));
        assert!(!native_delivery_receipted(&consumed, second));
    }

    #[test]
    fn a_graph_archive_moves_the_native_delivery_file() {
        let root = tempfile::tempdir().unwrap();
        let inbox = root.path().join("inbox");
        let archive = root.path().join("archive");
        let filename = st2::message::send_to_inbox(
            &inbox,
            "requester",
            Some("Start"),
            None,
            &["st3-message:message/kickoff/run-1".into()],
            "Do the work.",
        )
        .unwrap();
        let closed = BTreeSet::from(["message/kickoff/run-1".into()]);
        sync_consumed_projected_messages(&inbox, &archive, &closed).unwrap();

        assert!(!inbox.join(&filename).exists());
        assert!(archive.join(filename).is_file());
    }

    #[test]
    fn a_graph_read_releases_the_native_delivery_fifo() {
        let root = tempfile::tempdir().unwrap();
        let inbox = root.path().join("inbox");
        let archive = root.path().join("archive");
        let old = st2::message::send_to_inbox(
            &inbox,
            "requester",
            Some("Already read"),
            None,
            &["st3-message:message/old".into()],
            "The recipient read this through the graph.",
        )
        .unwrap();
        let next = st2::message::send_to_inbox(
            &inbox,
            "requester",
            Some("Still staged"),
            None,
            &["st3-message:message/next".into()],
            "This still needs native delivery.",
        )
        .unwrap();
        sync_consumed_projected_messages(&inbox, &archive, &BTreeSet::from(["message/old".into()]))
            .unwrap();
        sync_consumed_projected_messages(&inbox, &archive, &BTreeSet::from(["message/old".into()]))
            .unwrap();

        assert!(!inbox.join(&old).exists());
        assert!(archive.join(old).is_file());
        assert!(inbox.join(next).is_file());
        assert_eq!(st2::message::list_dir(&archive).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_closed_message_missing_from_the_active_page_archives_its_projected_file() {
        use axum::{Json, Router, routing::get};

        let app = Router::new()
            .route(
                "/v1/messages/page",
                get(|| async {
                    Json(serde_json::json!({
                        "api_version": "st3.v1",
                        "value": { "items": [], "has_more": false, "next_cursor": null, "limit": 100 }
                    }))
                }),
            )
            .route(
                "/v1/messages/read/{*subject}",
                get(|| async {
                    Json(serde_json::json!({
                        "api_version": "st3.v1",
                        "value": {
                            "subject": "message/closed", "from": "agent/sender", "to": "agent/test",
                            "content": "done", "status": "closed", "created_index": 1
                        }
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::new(Endpoint::Http(format!("http://{address}")));
        let root = tempfile::tempdir().unwrap();
        let inbox = root.path().join("inbox");
        let archive = root.path().join("archive");
        let filename = st2::message::send_to_inbox(
            &inbox,
            "agent/sender",
            Some("done"),
            None,
            &["st3-message:message/closed".into()],
            "done",
        )
        .unwrap();

        forward_projected_messages(
            &client,
            "agent/test",
            &inbox,
            &archive,
            "claude-channel",
            NativeDeliveryReceipts::ClaudeChannel {
                agent_dir: root.path(),
                incarnation: "one",
            },
        )
        .await
        .unwrap();
        assert!(!inbox.join(&filename).exists());
        assert!(archive.join(filename).is_file());
        server.abort();
    }

    #[tokio::test]
    async fn a_projected_message_envelope_names_the_graph_recipient_and_exact_body_hash() {
        use axum::{Json, Router, routing::get};

        let app = Router::new().route(
            "/v1/messages/page",
            get(|| async {
                Json(serde_json::json!({
                    "api_version": "st3.v1",
                    "value": {
                        "items": [{
                            "subject": "message/fact", "from": "agent/run-1/wake.left",
                            "to": "agent/run-1/wake.right", "content": "FACT <b>QUARTZ</b>",
                            "status": "staged", "title": "Fact", "created_index": 1
                        }],
                        "has_more": false, "next_cursor": null, "limit": 100
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::new(Endpoint::Http(format!("http://{address}")));
        let root = tempfile::tempdir().unwrap();
        let inbox = root.path().join("inbox");
        let archive = root.path().join("archive");

        forward_projected_messages(
            &client,
            "agent/run-1/wake.right",
            &inbox,
            &archive,
            "codex",
            NativeDeliveryReceipts::ClaudeChannel {
                agent_dir: root.path(),
                incarnation: "one",
            },
        )
        .await
        .unwrap();
        server.abort();

        let projected = st2::message::list_inbox(&inbox).unwrap();
        assert_eq!(projected.len(), 1);
        // The inbox file appends a newline, so the hash must come from the graph content.
        assert_eq!(projected[0].body, "FACT <b>QUARTZ</b>\n");
        let catalog = tempfile::tempdir().unwrap();
        assert_eq!(
            st2::ding::poke_text(catalog.path(), "h", "run-1/wake.right", &projected[0]),
            format!(
                "<smalltalk-message id=\"fact\" from=\"agent/run-1/wake.left\" \
                 to=\"agent/run-1/wake.right\" subject=\"Fact\" sha256=\"{}\" \
                 graph=\"message/fact\">\nFACT &lt;b&gt;QUARTZ&lt;/b&gt;\n</smalltalk-message>",
                st2::ding::st3_body_sha256("FACT <b>QUARTZ</b>")
            )
        );
    }

    #[tokio::test]
    async fn a_malformed_message_page_degrades_and_recovers_without_ending_delivery() {
        use axum::{Json, Router, response::IntoResponse as _, routing::get};

        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let get_calls = calls.clone();
        let post_observed = observed.clone();
        let app = Router::new()
            .route(
                "/v1/messages/page",
                get(move || {
                    let calls = get_calls.clone();
                    async move {
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            axum::response::Response::builder()
                                .status(200)
                                .body(axum::body::Body::from("{\"api_version\":\"st3.v1\",\"value\":\""))
                                .unwrap()
                        } else {
                            Json(serde_json::json!({
                                "api_version": "st3.v1",
                                "value": {"items": [], "has_more": false, "next_cursor": null, "limit": 100}
                            }))
                            .into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/claims",
                axum::routing::post(move |Json(body): Json<Value>| {
                    let observed = post_observed.clone();
                    async move {
                        observed.lock().unwrap().push(
                            body["fields"]["code"].as_str().unwrap().to_owned(),
                        );
                        Json(serde_json::json!({
                            "api_version": "st3.v1",
                            "value": {
                                "id":"claim/test", "store_index":1, "batch_id":"batch/test",
                                "subject":"agent/test", "kind":"harness.diagnostic", "origin":"test",
                                "actor":"agent/test", "body":{}, "predecessors":[],
                                "accepted_at_unix_ms":1
                            }
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::new(Endpoint::Http(format!("http://{address}")));
        let root = tempfile::tempdir().unwrap();
        let inbox = root.path().join("inbox");
        let archive = root.path().join("archive");
        let mut supervisor = NativeDeliverySupervisor::default();

        supervise_native_delivery(
            &client,
            "agent/test",
            &inbox,
            &archive,
            "claude-channel",
            NativeDeliveryReceipts::ClaudeChannel {
                agent_dir: root.path(),
                incarnation: "one",
            },
            "one",
            &mut supervisor,
        )
        .await;
        assert_eq!(supervisor.failures, 1);
        assert!(!supervisor.ready());
        supervisor.retry_after = None;
        supervise_native_delivery(
            &client,
            "agent/test",
            &inbox,
            &archive,
            "claude-channel",
            NativeDeliveryReceipts::ClaudeChannel {
                agent_dir: root.path(),
                incarnation: "one",
            },
            "one",
            &mut supervisor,
        )
        .await;
        assert_eq!(supervisor.failures, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            *observed.lock().unwrap(),
            ["native-delivery-degraded", "native-delivery-recovered"]
        );
        server.abort();
    }

    #[test]
    fn mission_follow_stops_for_completed_and_standing_runs() {
        assert!(mission_run_follow_succeeded("completed"));
        assert!(mission_run_follow_succeeded("standing"));
        assert!(!mission_run_follow_succeeded("running"));
        assert!(!mission_run_follow_succeeded("failed"));
    }

    #[test]
    fn a_runtime_driver_retries_a_transient_st3_api_outage() {
        let mut last_warning = None;
        tolerate_driver_api_outage(
            "agent/run/worker",
            anyhow::anyhow!("incomplete HTTP response"),
            &mut last_warning,
        )
        .unwrap();
        assert!(last_warning.is_some());
    }

    #[test]
    fn a_runtime_driver_retries_a_truncated_json_envelope() {
        let mut last_warning = None;
        let parse_error = serde_json::from_str::<Value>("\"").unwrap_err();
        tolerate_driver_api_outage(
            "agent/run/worker",
            anyhow::Error::new(parse_error).context("decode the st API response envelope"),
            &mut last_warning,
        )
        .unwrap();
        assert!(last_warning.is_some());
    }

    #[test]
    fn a_runtime_driver_does_not_retry_a_semantic_api_error() {
        let mut last_warning = None;
        let error = tolerate_driver_api_outage(
            "agent/run/worker",
            anyhow::anyhow!("the work claim is stale"),
            &mut last_warning,
        )
        .unwrap_err();
        assert!(error.to_string().contains("work claim is stale"));
        assert!(last_warning.is_none());
    }

    async fn serve_test_store(
        store: Arc<Store>,
        root: &Path,
        node: &str,
    ) -> (Client, tokio::task::JoinHandle<()>) {
        let socket = root.join("st3.sock");
        let state = AppState {
            store,
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: node.into(),
            state_dir: root.to_path_buf(),
            pty_root: root.join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: PlannerSpec::default(),
        };
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            serve_unix(&server_socket, router(state)).await.unwrap();
        });
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(socket.exists(), "the test API socket did not start");
        (Client::unix(&socket), server)
    }

    async fn publish_test_mission(client: &Client, root: &Path, goal: &str) {
        let file = root.join("mission.kdl");
        fs::write(
            &file,
            format!(
                "version 2\nmission \"arrival\" state=\"ready\" {{\n  goal \"{goal}\"\n  step \"work\" {{ }}\n}}\n"
            ),
        )
        .unwrap();
        publish_mission_file(
            client,
            MissionPublishArgs {
                file,
                at_index: None,
                actor: "person/test".into(),
            },
            true,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn missions_start_waits_for_a_revision_published_on_another_host() {
        const FLEET: &str = "5d0c1c52-3f7a-4b0e-9b61-1f2d3c4b5a69";
        let publisher_root = tempfile::tempdir().unwrap();
        let starter_root = tempfile::tempdir().unwrap();
        let publisher = Arc::new(Store::open_memory("publisher").unwrap());
        let starter = Arc::new(Store::open_memory("starter").unwrap());
        publisher.bind_fleet(FLEET).unwrap();
        starter.bind_fleet(FLEET).unwrap();
        let (publisher_client, publisher_server) =
            serve_test_store(publisher.clone(), publisher_root.path(), "publisher").await;
        let (starter_client, starter_server) =
            serve_test_store(starter.clone(), starter_root.path(), "starter").await;
        publish_test_mission(
            &publisher_client,
            publisher_root.path(),
            "Start after replication.",
        )
        .await;
        let revision = publisher
            .mission_spec("arrival", None)
            .unwrap()
            .unwrap()
            .revision;

        let replicate = tokio::spawn({
            let publisher = publisher.clone();
            let starter = starter.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(600)).await;
                let exchange = publisher
                    .export_replication_exchange(
                        FLEET,
                        &st3::model::ReplicationInventory::default(),
                    )
                    .unwrap();
                starter
                    .receive_replication_exchange("publisher", FLEET, &exchange)
                    .unwrap();
                starter.validate_replication_backlog().unwrap();
                starter.apply_replication_repairs().unwrap();
                starter.project_replication_backlog().unwrap();
            }
        });
        let waited = Instant::now();
        start_mission_run(
            &starter_client,
            MissionRunStartArgs {
                mission: "arrival".into(),
                revision: Some(revision.clone()),
                id: Some("arrival/after-replication".into()),
                workspace: starter_root.path().to_path_buf(),
                inputs: Vec::new(),
                after: None,
                follow: false,
                actor: "person/test".into(),
                print_kdl: false,
            },
            true,
        )
        .await
        .unwrap();
        assert!(
            waited.elapsed() >= Duration::from_millis(500),
            "start waited for the publish to replicate instead of failing"
        );
        replicate.await.unwrap();
        let run: MissionRunView = starter_client
            .get("/v1/mission-runs/mission-run%2Farrival%2Fafter-replication")
            .await
            .unwrap();
        assert_eq!(run.revision, revision);
        publisher_server.abort();
        starter_server.abort();
    }

    #[tokio::test]
    async fn missions_start_names_a_replaced_or_missing_revision() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_memory("single").unwrap());
        let (client, server) = serve_test_store(store.clone(), root.path(), "single").await;
        publish_test_mission(&client, root.path(), "First revision.").await;
        let first = store
            .mission_spec("arrival", None)
            .unwrap()
            .unwrap()
            .revision;
        publish_test_mission(&client, root.path(), "Second revision.").await;
        let second = store
            .mission_spec("arrival", None)
            .unwrap()
            .unwrap()
            .revision;
        assert_ne!(first, second);

        let replaced = Instant::now();
        let error = startable_mission(&client, "arrival", Some(&first), Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(&format!(
                "revision {first} was replaced by revision {second}"
            )),
            "{error}"
        );
        assert!(replaced.elapsed() < Duration::from_secs(5));

        let error = startable_mission(
            &client,
            "arrival",
            Some(&"0".repeat(64)),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("has not reached this host yet after 1s"),
            "{error}"
        );
        let error = startable_mission(&client, "absent", None, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("mission/absent has not reached this host yet after 1s"),
            "{error}"
        );

        let publications = mission_publications(&client, "arrival").await.unwrap();
        assert_eq!(publications.len(), 2);
        assert!(
            started_revision_note("arrival", &second, &publications)
                .ends_with("on single. 1 older revision shares this mission name.")
        );
        assert!(
            started_revision_note("arrival", &first, &publications)
                .ends_with("on single. 1 other revision shares this mission name.")
        );
        assert_eq!(
            started_revision_note("arrival", &second, &publications[..1]),
            format!("Started mission/arrival revision {second}.")
        );
        server.abort();
    }
}
