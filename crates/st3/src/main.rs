use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use clap::{Args, CommandFactory as _, FromArgMatches as _, Parser, Subcommand, ValueEnum};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use st3::api::{AppState, fabric_router, router, serve_unix};
use st3::client::{Client, Endpoint};
use st3::config::{Config, PeerConfig, validate_unix_socket_path};
use st3::model::{
    ApplyRequest, ApplyResponse, AttachRequest, Attachment, AttentionItemView, AttentionRequest,
    AttentionRequestView, AttentionResolveRequest, AttentionWithdrawRequest, ClaimInput,
    ClaimRecord, ClaimsPage, CurrentHarnessView, DoctorReport, DocumentListResponse,
    DocumentPutRequest, DocumentVersion, EvalStatus, EventRecord, IntentInput,
    LaunchApproveAndStartRequest, LaunchApproveAndStartView, LaunchDecisionAnswerRequest,
    LaunchDecisionOption, LaunchDecisionRequest, LaunchDecisionResponse, LaunchDecisionType,
    LaunchStartRequest, MessageLifecycleRequest, MessagePage, MessageSendReceipt,
    MessageSendRequest, MessageView, MissionOutputView, MissionProductionRequest, MissionRequest,
    MissionResponse, MissionRetireRequest, MissionRevisionRequest, MissionRunOutcomeRequest,
    MissionRunView, MissionSpec, MissionState, OperationalRepairApplyRequest,
    OperationalRepairPlan, OperationalRepairResult, PersonAskRequest, PersonStepResponse,
    PlannerSpec, PlanningApprovalRequest, PlanningCandidateSubmitRequest, PlanningProposalRequest,
    PlanningSessionView, ReplicaRecordView, ReplicationPeerStatus, ReplicationRepairRequest,
    ReplicationStatus, ReviewRequest, RevisionApprovalRequest, RevisionCancelRequest,
    RevisionProposalView, RevisionSubmissionView, RunGenerationView, SessionControlResponse,
    SessionInputMode, SessionInputRequest, SessionScreen, SessionSignalRequest, StatusResponse,
    StepRunView, SubscriptionRequestDecision, SubscriptionRequestView, WorkExtendRequest,
    WorkRequest, WorkRetryRequest, WorkWakeRequest,
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

mod cli_help;
mod presentation;

use presentation::{
    OutputStyle, follow_snapshot, glance, mission_run_signature, relative_time,
    render_attention_show, render_generation, render_generations, render_host_facts,
    render_human_value, render_mission_run, render_revision_proposal, render_step_run,
    shell_argument,
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
    /// Atomically publish a complete owned set of seats, missions and schedules.
    Apply(OwnedSetApplyArgs),
    /// Inspect owned sets and their source publication receipts.
    Sets {
        #[command(subcommand)]
        command: OwnedSetsCommand,
    },
    /// Start the HTTP API, readers, peers, and reconciler.
    Up(UpArgs),
    /// Understand what needs action now.
    Now(NowArgs),
    /// Show token spend over a period, with the largest spenders first.
    Usage(UsageArgs),
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
    #[command(
        after_help = "Messages never appear here; read them with `st conversations`.\nFaults never appear here; st sends each one to the agent that owns it, which asks a person with `st work ask` only if it needs to."
    )]
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
    /// Show and change the ordered lanes that mission runs work through, such as a merge train.
    Lanes {
        #[command(subcommand)]
        command: LaneCommand,
    },
    /// Inspect and control terminal members.
    Terminals {
        #[command(subcommand)]
        command: PtyCommand,
    },
    /// Check the daemon and runtime dependencies.
    Doctor(DoctorArgs),
    /// Summarize git and gh command logs.
    Recorder {
        #[command(subcommand)]
        command: RecorderCommand,
    },
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
    /// Watch GitHub issues and pull requests: each comment, review, required-check result and
    /// close wakes this seat once.
    Gh {
        #[command(subcommand)]
        command: GhCommand,
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
    /// Upload or read the images a message carries between machines.
    Blobs {
        #[command(subcommand)]
        command: BlobCommand,
    },
    /// Restrict what agents may write, and read what each rule would have refused.
    Rules {
        #[command(subcommand)]
        command: RuleCommand,
    },
    /// Discover native harness sessions and move one under durable st ownership.
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    /// Generate one shell completion script.
    Completions(CompletionsArgs),
    /// Answer one built-in gate check, as built-in gates run it: exit 0 to pass, 1 for not yet,
    /// and 3 when the check cannot answer.
    Gate {
        #[command(subcommand)]
        command: GateCommand,
    },
    /// Print the st agent skill bundled in this binary, or install it for each harness.
    Skill(SkillArgs),
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
    /// Let --peer-listen bind a non-loopback, non-Tailscale address; traffic is plain HTTP.
    #[arg(long)]
    peer_listen_allow_plain_http: bool,
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
    /// Wait for this node's first sync to end, then check that it projects the same graph as
    /// the peer it synced from. Fails when the graphs differ after a heal.
    Wait {
        /// How long to wait, like 90s or 30m.
        #[arg(long, default_value = "30m")]
        timeout: String,
    },
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
    /// Accept no inbound connections; replication and presence otherwise behave normally.
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
    /// A loopback/tailnet http:// or fabric:// route to the sponsor.
    #[arg(long)]
    via: Option<String>,
    /// Do not stop, install, or start services; print the foreground commands instead.
    #[arg(long)]
    no_service: bool,
    /// Return once the services start, instead of waiting for the first sync to end.
    #[arg(long)]
    no_wait: bool,
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
            let services = !args.no_service && services_installed();
            if services {
                st3::service::install(Config::load_with_fleet(None)?)?;
            }
            print!("{}", cli_help::fleet_created_next_steps(services));
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
                runtime: st3::store::runtime(),
                version: env!("CARGO_PKG_VERSION").into(),
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
            let mut sync_state = None;
            if use_services {
                st3::service::install(Config::load_with_fleet(None)?)?;
                if !json_output {
                    println!("The st services now run as a fleet member.");
                }
                if !args.no_wait {
                    match wait_for_first_sync(&client, Duration::from_secs(30 * 60), !json_output)
                        .await?
                    {
                        // The join already printed its JSON; a failed first sync still fails.
                        Some(first) if json_output => anyhow::ensure!(
                            first.state == "verified",
                            "{}",
                            render_first_sync(&first, now_ms())
                        ),
                        Some(first) => {
                            sync_state = Some(first.state.clone());
                            if first.state != "verified" {
                                print!(
                                    "{}",
                                    cli_help::fleet_next_steps(use_services, sync_state.as_deref())
                                );
                            }
                            report_first_sync(&first, false)?;
                        }
                        None if json_output => {}
                        None => println!(
                            "This machine is a member and still syncing; st fleet wait waits for \
                             the first sync to end and checks it."
                        ),
                    }
                }
            } else if !json_output {
                println!(
                    "Start st3 up and st3 replication-worker to begin syncing; st fleet wait \
                     waits for the first sync to end and checks it."
                );
            }
            if !json_output {
                print!(
                    "{}",
                    cli_help::fleet_next_steps(use_services, sync_state.as_deref())
                );
            }
            Ok(())
        }
        FleetCommand::Wait { timeout } => {
            let timeout = Duration::from_secs(parse_fleet_duration(&timeout)?);
            let started = std::time::Instant::now();
            let since = now_ms();
            let first = wait_for_first_sync(&client, timeout, !json_output)
                .await?
                .with_context(|| {
                    format!(
                        "the first sync has not ended after {} s; st replication status shows \
                         how far it got",
                        timeout.as_secs()
                    )
                })?;
            if first.state != "verified" {
                return report_first_sync(&first, json_output);
            }
            // A first sync is verified once, long ago after a restart. A wait is a gate for now:
            // it also needs an exchange since it began at which this node held everything.
            let caught_up = wait_for_caught_up(
                &client,
                since,
                timeout.saturating_sub(started.elapsed()),
                !json_output,
            )
            .await?;
            let caught_up = match caught_up {
                Ok(caught_up) => caught_up,
                Err(waiting) => anyhow::bail!(
                    "{}\nbut this node has not caught up since this wait began {} s ago: {}; st \
                     replication status shows how far it got",
                    render_first_sync(&first, now_ms()),
                    started.elapsed().as_secs(),
                    waiting
                ),
            };
            if json_output {
                let mut value = serde_json::to_value(&first)?;
                value["caught_up"] = serde_json::to_value(&caught_up)?;
                return print_value(&value, true);
            }
            println!("{}", render_first_sync(&first, now_ms()));
            println!("{}", render_caught_up(&caught_up, now_ms()));
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
            if let Some(removed) = &status.removed {
                println!("REMOVED  {}", removed.describe(status.fleet_id.as_deref()));
            }
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
                    peer.refusal_reason
                        .as_deref()
                        .or(peer.last_error.as_deref())
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
    // The member key signed for this node and the people and agents it held keys for; a node
    // that joins again starts with new keys.
    let keys = st3::fleet::join::key_directory(&config.state_dir);
    if keys.exists() {
        fs::remove_dir_all(keys)?;
    }
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
    /// A loopback/tailnet http:// or fabric:// route to the sponsor.
    #[arg(long, conflicts_with_all = ["anchor", "finish", "unfinish"])]
    via: Option<String>,
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
            st3::store::runtime(),
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
            via: args.via.clone(),
            settings,
            legacy_secret_file: Some(secret_file),
            fabric_protocol: args.fabric_protocol.clone(),
            runtime: st3::store::runtime(),
            version: env!("CARGO_PKG_VERSION").into(),
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
    /// Let --peer-listen bind a non-loopback, non-Tailscale address; traffic is plain HTTP.
    #[arg(long)]
    peer_listen_allow_plain_http: bool,
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
        /// Follow current collection changes.
        #[arg(long, conflicts_with_all = ["all", "cursor", "since", "until", "status"])]
        watch: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Terminal transitions since a duration such as 6h, or an RFC3339 timestamp.
        #[arg(long)]
        since: Option<String>,
        /// End of the time window (RFC3339 timestamp or duration ago).
        #[arg(long)]
        until: Option<String>,
        /// Terminal state: failed, cancelled, timed-out, or completed. Includes reasons.
        #[arg(long, value_parser = ["failed", "cancelled", "timed-out", "completed"])]
        status: Option<String>,
    },
    /// Explain one mission run, its goals, state, work, and usage.
    Show(MissionShowArgs),
    /// Publish exact authored mission KDL after preview, once its exec gates pass a check.
    ///
    /// Goals, constraints and named documents encode every known rule and decision.
    /// `depends-on` orders steps; `missions start --after` orders runs without reports.
    /// A final step assigned to the author, depending on the last real step, reaches the
    /// author once when work is done. Review gates mark decisions only a person can make.
    Publish(MissionPublishArgs),
    /// Run each exec gate in a mission file once, now, the way a run would, and report its
    /// answer: pass (exit 0), not yet (exit 1), broken (anything else), or unchecked.
    Check(MissionCheckArgs),
    /// Start one run from the current ready mission revision.
    ///
    /// Goals, constraints and named documents encode every known rule and decision.
    /// `depends-on` orders steps; `--after` orders runs without reports.
    /// A final step assigned to the author, depending on the last real step, reaches the
    /// author once when work is done. Review gates mark decisions only a person can make.
    Start(MissionRunStartArgs),
    /// Cancel one exact running mission and stop its owned work and runtimes.
    Cancel(MissionCancelArgs),
    /// Set a finished run's outcome to completed, failed, or cancelled, with a reason.
    Outcome(MissionOutcomeArgs),
    /// Retire a mission so it leaves the lists and cannot start; publishing it again brings it back.
    Retire(MissionRetireArgs),
    /// Show one seat's current claim and its queued mission runs in order; same as `st agents queue AGENT`.
    Queued {
        /// Exact seat subject or its identity without the `agent/` prefix.
        agent: String,
    },
    /// Show one subscription's automatic mission request queue.
    Requests {
        subscription: String,
        /// Include started, cancelled, and failed requests.
        #[arg(long)]
        all: bool,
    },
    /// Release a legacy held request; new requests are queued automatically.
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
    /// The workspace a run would use, where publish first checks each exec gate.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Publish without first running each exec gate once to refuse a broken one.
    #[arg(long)]
    no_gate_check: bool,
}

#[derive(Args)]
struct MissionCheckArgs {
    /// KDL file to check; use `-` to read standard input.
    file: PathBuf,
    /// The workspace a run would use: `${ST_WORKSPACE}` and relative gate workspaces stand for
    /// it.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// A mission input value for the check, as `missions start` takes it. A gate that reads an
    /// input without one is unchecked.
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
}

#[derive(Subcommand)]
enum GateCommand {
    /// Whether a pull request has merged; one that closed unmerged cannot pass.
    Merged {
        /// OWNER/REPO#NUMBER.
        pull_request: String,
    },
    /// Whether the check run or commit status named CHECK passed on a commit or branch head.
    CiPassed {
        check: String,
        /// OWNER/REPO.
        #[arg(long)]
        repo: String,
        /// A commit SHA or a branch name.
        #[arg(long = "ref")]
        reference: String,
    },
    /// Whether a cargo test target passes at a ref, built in a worktree st keeps between checks.
    CargoTest {
        /// The test target, as `cargo test --test TARGET` names it.
        target: String,
        #[arg(long)]
        package: String,
        /// The ref to test; its remote is fetched first.
        #[arg(long = "ref", default_value = "origin/main")]
        reference: String,
        /// The repository, or a directory inside it.
        #[arg(long, default_value = ".")]
        repository: PathBuf,
        /// The worktree to build in; st keeps one beneath its state directory by default.
        #[arg(long)]
        worktree: Option<PathBuf>,
    },
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
    /// Person or authorized agent carried over the trusted local Unix boundary.
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
}

#[derive(Args)]
struct MissionOutcomeArgs {
    /// Exact mission-run subject of a finished run.
    mission_run: String,
    /// The outcome the run should show.
    #[arg(value_parser = ["completed", "failed", "cancelled"])]
    status: String,
    /// Why the run has this outcome, for example that its work shipped after a gate failed.
    #[arg(long)]
    reason: String,
    /// A person, or an agent that requested the run or may revise its mission.
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
}

#[derive(Args)]
struct MissionRetireArgs {
    /// Mission to retire, such as `mission/fleet/demo/deploy`.
    mission: String,
    /// A person, or an agent that may publish the mission.
    #[arg(long = "as", value_parser = parse_publication_actor)]
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
    #[arg(long = "as", value_parser = parse_actor_subject)]
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
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
    /// Print the feedback KDL without storing the feedback or publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct PlanningApproveArgs {
    session: String,
    preview_hash: String,
    #[arg(long = "as", value_parser = parse_actor_subject)]
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
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args)]
struct LaunchRunArgs {
    session: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
    #[arg(long = "as", value_parser = parse_actor_subject)]
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
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args)]
struct PlanningCancelArgs {
    session: String,
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
    #[arg(long)]
    reason: Option<String>,
    /// Print the cancellation KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Subcommand)]
enum PtyCommand {
    /// Open a plain shell for the configured person, without an agent harness.
    New(PtyNewArgs),
    /// End a personal shell permanently, including its durable declaration.
    End(PtyScreenArgs),
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
    ///
    /// A terminal on this host is attached through its PTY session, fenced to the incarnation st
    /// selected, so a busy daemon cannot block it. A terminal on another fleet host is attached
    /// PTY to PTY over Fabric when Fabric reaches that host (see `st terminals expose-fabric`),
    /// and otherwise through the client gateway, the path paired clients use, as the person from
    /// `--as` or the st config. Attaching never starts or restarts a session. Detach with Ctrl+\.
    Attach(PtyAttachArgs),
    /// Let fleet peers attach this host's terminals PTY to PTY over Fabric.
    ///
    /// Fabric keeps the exposure in its own configuration and runs `st terminals serve-fabric`
    /// for each tunnel, so an attach works while st's daemon is busy or down. Each peer also needs
    /// a grant for the printed protocol in this machine's Fabric `peers.toml`.
    ExposeFabric(PtyExposeFabricArgs),
    /// Serve one Fabric tunnel to a PTY session on stdin and stdout. Fabric runs this.
    #[command(hide = true)]
    ServeFabric(PtyServeFabricArgs),
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
struct PtyNewArgs {
    /// Display name; defaults to a generated name.
    name: Option<String>,
    #[arg(long)]
    host: Option<String>,
    /// Absolute directory on the selected host. Locally defaults to the caller's directory;
    /// remotely defaults to the daemon's directory.
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtySubjectArgs {
    subject: String,
}

#[derive(Args)]
struct PtyScreenArgs {
    subject: String,
    /// Use this concrete person instead of the person configured for trusted local commands.
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtyStreamArgs {
    subject: String,
    #[arg(long = "as", value_parser = parse_actor_subject)]
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
    #[arg(long = "as", value_parser = parse_actor_subject)]
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
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtyAttachArgs {
    subject: String,
    /// Allow an attachment from inside another PTY session.
    #[arg(long)]
    force: bool,
    /// The person attaching to a terminal another fleet host owns; defaults to `person` in the st
    /// config. A terminal on this host needs no person.
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtyExposeFabricArgs {
    /// The st executable Fabric runs for each tunnel; defaults to this one. Name a stable path,
    /// since Fabric keeps it after this build is replaced.
    #[arg(long)]
    st: Option<PathBuf>,
}

#[derive(Args)]
struct PtyServeFabricArgs {
    /// Serve the tunnel on stdin and stdout, as Fabric's exec exposure runs it.
    #[arg(long, required = true)]
    stdio: bool,
    /// The PTY root whose sessions this serves.
    #[arg(long)]
    pty_root: PathBuf,
}

#[derive(Args)]
struct PtySendArgs {
    subject: String,
    /// Text typed as one line and followed by Enter a moment later; with --raw the exact bytes,
    /// with --key a key name.
    value: String,
    /// Send exactly these bytes and no Enter: escape sequences, such as a mouse report.
    #[arg(long, conflicts_with = "key")]
    raw: bool,
    /// Send one named key, such as enter, escape or ctrl+c.
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

#[derive(Clone, Copy, ValueEnum)]
enum UsageBy {
    Agent,
    Mission,
    Step,
    Model,
    Account,
    Host,
}

impl UsageBy {
    const ALL: [Self; 6] = [
        Self::Agent,
        Self::Mission,
        Self::Step,
        Self::Model,
        Self::Account,
        Self::Host,
    ];

    /// The report row field this grouping reads.
    fn field(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Mission => "mission_run",
            Self::Step => "step",
            Self::Model => "model",
            Self::Account => "account",
            Self::Host => "host",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Mission => "mission",
            other => other.field(),
        }
    }
}

#[derive(Args)]
struct UsageArgs {
    /// Length of the period ending now.
    #[arg(long, default_value_t = 24)]
    hours: u64,
    /// Show only this grouping; the default shows them all.
    #[arg(long, value_enum)]
    by: Option<UsageBy>,
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
    Show(SubjectShowArgs),
    /// Show bounded immutable history for one subject.
    History(TraceArgs),
}

#[derive(Args)]
struct SubjectShowArgs {
    subject: String,
    /// Print the current managed agent declaration as canonical KDL v2.
    #[arg(long)]
    kdl: bool,
    /// Include literal environment values in the KDL output.
    #[arg(long, requires = "kdl")]
    show_env_values: bool,
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
    /// Show only the slowest requests and queries over the last five minutes.
    #[arg(long)]
    performance: bool,
}

#[derive(Subcommand)]
enum RecorderCommand {
    /// Summarize recent calls from local or supplied host logs.
    Report(RecorderReportArgs),
}

#[derive(Args)]
struct RecorderReportArgs {
    /// Include calls from the last number of hours.
    #[arg(long, default_value_t = 24)]
    hours: u64,
    /// Read this JSONL log. Repeat for logs copied from other hosts; defaults to this host's log.
    #[arg(long = "log")]
    logs: Vec<PathBuf>,
    /// Number of slow calls to show.
    #[arg(long, default_value_t = 10)]
    top: usize,
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
    /// Plan checkpoints that trim replicated history.
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
}

#[derive(Subcommand)]
enum CheckpointCommand {
    /// Show what a checkpoint would drop from this node's store, proved on a copy. Changes nothing.
    Plan {
        /// The UTC day that names the checkpoint, such as 2026-09-27. Defaults to the newest due one.
        #[arg(long)]
        cut: Option<String>,
    },
    /// Show the newest stable checkpoint and who has sealed or verified the one being agreed.
    Status,
    /// Stop waiting for an unreachable writer. It fences nothing: what the writer wrote while
    /// away still replicates when it returns, and its next seal ends the excusal.
    Excuse {
        /// The writer, as its node name.
        writer: String,
        #[arg(long)]
        reason: String,
        /// The person excusing it.
        #[arg(long = "as")]
        actor: Option<String>,
    },
    /// Go on with checkpoints after a trim stopped because deleting would change the graph.
    /// The rows stay as they are; the next checkpoint proceeds.
    Resume {
        /// What the person found.
        #[arg(long)]
        reason: String,
        /// The person resuming.
        #[arg(long = "as")]
        actor: Option<String>,
    },
}

#[derive(Subcommand)]
enum BlobCommand {
    /// Keep one PNG, JPEG, GIF or WebP image (at most 10 MiB) on this member and print its
    /// reference. Name it in `conversations send --attach`, or pass the file there directly.
    Put {
        file: PathBuf,
        /// The image type; by default the file's own bytes decide.
        #[arg(long)]
        media_type: Option<String>,
        /// Who uploads; defaults to ST_AGENT, then the configured person.
        #[arg(long = "as")]
        actor: Option<String>,
    },
    /// Read one attachment. A member that does not hold it asks the member that took the upload.
    Get {
        /// `blob/<sha256>` or the hash.
        reference: String,
        /// The message that carries it; a person reads an attachment through its message.
        #[arg(long)]
        message: Option<String>,
        /// Write the image here instead of standard output.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Who reads; defaults to ST_AGENT, then the configured person.
        #[arg(long = "as")]
        actor: Option<String>,
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
enum RuleCommand {
    /// List the rules, each with its mode: off, audit or enforce.
    Ls,
    /// List the writes the rules in audit mode would have refused, newest first.
    Audit {
        /// Only this rule's records.
        rule: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Set the lockdown rules, each in audit mode until you enforce it.
    Lockdown {
        #[arg(long = "as")]
        actor: Option<String>,
        /// An agent that may still start agents, such as agent/example/planner. Repeatable.
        #[arg(long = "starter")]
        starters: Vec<String>,
    },
    /// Turn one rule off, to audit, or to enforce.
    Mode {
        name: String,
        #[arg(value_parser = ["off", "audit", "enforce"])]
        mode: String,
        #[arg(long = "as")]
        actor: Option<String>,
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
        #[arg(long = "as", value_parser = parse_actor_subject)]
        person: String,
    },
}

#[derive(Args)]
struct AgentsArgs {
    /// Follow current collection changes.
    #[arg(long, conflicts_with_all = ["all", "cursor"])]
    watch: bool,
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
    /// Declare one new agent seat, start its harness, and wait until it is ready.
    ///
    /// The declaration is the one a person writes by hand, with the harness defaults of the
    /// fleet's Claude and Codex seats; `--print-kdl` shows it without applying it. With
    /// `--attach`, this terminal attaches once the harness is ready, from any fleet host.
    New(AgentNewArgs),
    /// Preview and apply one KDL file containing durable agent seats.
    Apply(AgentApplyArgs),
    /// Start a durable seat, patching only explicitly supplied declaration fields. A stopped
    /// mission seat starts again on its run's own declaration.
    Start(AgentStartArgs),
    /// Stop one exact durable seat and every process it started.
    ///
    /// The seat's builds and tests end with it, even those that left its harness's process tree.
    /// A host without a systemd user manager ends only the harness's process tree and process
    /// groups; docs/st3/priority.md says what else keeps running there.
    Stop(AgentStopArgs),
    /// Restart a top-level or mission seat, preserving its declaration; wait for a new incarnation.
    Restart(AgentRestartArgs),
    /// Stop a quiet seat at a clean boundary, keeping its native session to resume.
    ///
    /// The seat must be idle, with no pending ask or unsent input, no claimed step and no
    /// running subagent. A suspended seat stays declared, takes no restarts, and keeps its mail
    /// until it is resumed.
    Suspend(AgentSuspendArgs),
    /// Resume a suspended seat on the same native session; wait until its harness proves it.
    Resume(AgentResumeArgs),
    /// Change only a seat's human label, without restarting its harness.
    Rename(AgentRenameArgs),
    /// Show one seat's current claim and its queued mission runs in order, or move a run.
    /// The show form is also available as `st missions queued AGENT`.
    Queue(AgentQueueArgs),
    /// Inspect, set, or release a Codex/OpenCode delivery hold; the provider keeps running.
    Hold(AgentHoldArgs),
}

#[derive(Args)]
struct AgentHoldArgs {
    subject: String,
    /// Hold new native handoffs for this duration, such as 15m.
    #[arg(long = "for", conflicts_with = "release")]
    duration: Option<String>,
    #[arg(long)]
    release: bool,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct AgentRenameArgs {
    subject: String,
    #[arg(
        required_unless_present = "clear",
        conflicts_with = "clear",
        value_parser = clap::builder::NonEmptyStringValueParser::new()
    )]
    label: Option<String>,
    /// Restore the subject-derived presentation label.
    #[arg(long)]
    clear: bool,
    #[arg(long = "as")]
    actor: Option<String>,
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
    /// Person or agent making the move; defaults to `person` in the st config. Any agent may move
    /// any seat's queue, its own included.
    #[arg(long = "as", value_parser = parse_queue_move_actor)]
    actor: Option<String>,
}

#[derive(Subcommand)]
enum LaneCommand {
    /// List open lanes; `--all` also lists lanes whose run ended.
    Ls {
        #[arg(long)]
        all: bool,
    },
    /// Show one lane's entries in order and its recent changes.
    Show {
        /// A `lane/RUN/NAME` subject, a run or mission with one lane, or a unique lane name.
        lane: String,
    },
    /// Add an entry at the back of a lane. An entry already in the lane stays where it is.
    Join(LaneEntryArgs),
    /// Take an entry out of a lane.
    Leave(LaneLeaveArgs),
    /// Move an entry to the top, to the bottom, or next to another entry.
    Move(LaneMoveArgs),
    /// Record the status the lane's run found for an entry.
    Mark(LaneMarkArgs),
    /// Approve an entry as the lane's approver.
    Approve(LaneEntryArgs),
}

#[derive(Args)]
struct LaneEntryArgs {
    /// A `lane/RUN/NAME` subject, a run or mission with one lane, or a unique lane name.
    lane: String,
    /// The entry subject, or the part after the lane's entry prefix, such as a pull request number.
    entry: String,
    /// Why; recorded with the change.
    #[arg(long)]
    reason: Option<String>,
    /// Person or agent making the change. A harness acts as its own seat (`ST_AGENT`); otherwise
    /// this defaults to `person` in the st config.
    #[arg(long = "as", value_parser = parse_queue_move_actor)]
    actor: Option<String>,
}

#[derive(Args)]
struct LaneLeaveArgs {
    #[command(flatten)]
    entry: LaneEntryArgs,
    /// `completed` when the lane's work for the entry is done, or `removed` when it was dropped.
    #[arg(long, default_value = "removed", value_parser = ["completed", "removed"])]
    outcome: String,
}

#[derive(Args)]
#[command(group(
    clap::ArgGroup::new("placement")
        .required(true)
        .args(["top", "bottom", "before", "after"])
))]
struct LaneMoveArgs {
    #[command(flatten)]
    entry: LaneEntryArgs,
    /// Put the entry first.
    #[arg(long)]
    top: bool,
    /// Put the entry last.
    #[arg(long)]
    bottom: bool,
    /// Put the entry directly before another entry.
    #[arg(long, value_name = "ENTRY")]
    before: Option<String>,
    /// Put the entry directly after another entry.
    #[arg(long, value_name = "ENTRY")]
    after: Option<String>,
}

#[derive(Args)]
struct LaneMarkArgs {
    /// A `lane/RUN/NAME` subject, a run or mission with one lane, or a unique lane name.
    lane: String,
    /// The entry subject, or the part after the lane's entry prefix.
    entry: String,
    /// What the run found: waiting, held, ready, or running.
    #[arg(long, value_parser = ["waiting", "held", "ready", "running"])]
    state: String,
    /// A short explanation shown next to the status.
    #[arg(long)]
    detail: Option<String>,
    /// The exact head or version the status applies to.
    #[arg(long)]
    head: Option<String>,
    /// Person or agent recording the status. A harness acts as its own seat (`ST_AGENT`).
    #[arg(long = "as", value_parser = parse_queue_move_actor)]
    actor: Option<String>,
}

#[derive(Args)]
struct OwnedSetApplyArgs {
    #[arg(long)]
    set: String,
    /// Complete list of input KDL files; '-' reads stdin once.
    files: Vec<PathBuf>,
    #[arg(long)]
    repository: String,
    #[arg(long = "ref")]
    source_ref: String,
    #[arg(long)]
    sha: String,
    #[arg(long)]
    source_sequence: u64,
    /// 'absent' for initial creation, otherwise the previous selected set revision.
    #[arg(long)]
    expect_set: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long = "adopt")]
    adopt: Vec<String>,
    #[arg(long)]
    allow_empty: bool,
    #[arg(long)]
    confirm_retire: Option<String>,
    #[arg(long = "as", env = "ST_AGENT", value_parser = parse_publication_actor)]
    actor: String,
}

#[derive(Subcommand)]
enum OwnedSetsCommand {
    /// List selected owned sets and their source receipts.
    Ls,
    /// Show a set's live membership, retirements and blockers.
    Show {
        name: String,
    },
    /// Inspect publication and rollout for one source commit.
    Status {
        name: String,
        #[arg(long)]
        sha: String,
    },
}

async fn run_owned_set_apply(
    client: &Client,
    args: OwnedSetApplyArgs,
    json_output: bool,
) -> Result<()> {
    use st3::store::owned_sets::{Options, Preview, Request, Source};
    anyhow::ensure!(
        !args.files.is_empty() || args.allow_empty,
        "no input files: intentional empty membership needs --allow-empty"
    );
    anyhow::ensure!(
        args.files.iter().filter(|p| p.as_os_str() == "-").count() <= 1,
        "stdin may appear only once"
    );
    let mut bundle = String::from("version 2\n");
    for path in &args.files {
        let (text, _) = read_intent(Some(path))?;
        let mut doc: kdl::KdlDocument = text
            .parse()
            .with_context(|| format!("parse {}", path.display()))?;
        anyhow::ensure!(
            doc.nodes()
                .first()
                .is_some_and(|n| n.name().value() == "version"
                    && n.get(0).and_then(|v| v.as_integer()) == Some(2)),
            "{} must begin with version 2",
            path.display()
        );
        doc.nodes_mut().remove(0);
        bundle.push_str(&doc.to_string());
        bundle.push('\n');
    }
    let options = Options {
        set: args.set,
        source: Source {
            repository: args.repository,
            r#ref: args.source_ref,
            sha: args.sha,
            sequence: args.source_sequence,
        },
        expected_set: args.expect_set,
        adopt: args.adopt.into_iter().collect(),
        allow_empty: args.allow_empty,
        confirm_retire: args.confirm_retire,
        expected_subjects: Default::default(),
    };
    let mut request = Request {
        intent: IntentInput {
            kdl: bundle,
            source_name: Some("owned set input files".into()),
        },
        options,
        actor: args.actor,
        idempotency_key: uuid::Uuid::now_v7().to_string(),
    };
    let preview: Preview = client.post("/v1/sets/preview", &request).await?;
    if args.dry_run {
        return print_value(&preview, json_output);
    }
    anyhow::ensure!(
        preview.blockers.is_empty(),
        "owned set refused: {}",
        preview.blockers.join("; ")
    );
    request.options.expected_subjects = preview.expected_subjects;
    let response: Value = client.post("/v1/sets/apply", &request).await?;
    print_value(&response, json_output)
}

async fn run_owned_sets(
    endpoint: &Endpoint,
    command: OwnedSetsCommand,
    json_output: bool,
) -> Result<()> {
    let client = generated_client(endpoint, None)?;
    match command {
        OwnedSetsCommand::Ls => {
            print_value(&client.sets_list(None, None, false).await?, json_output)
        }
        OwnedSetsCommand::Show { name } => print_value(&client.sets_get(&name).await?, json_output),
        OwnedSetsCommand::Status { name, sha } => {
            print_value(&client.sets_status(&name, &sha).await?, json_output)
        }
    }
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
    /// Seat identity or complete subject, e.g. example/worker or agent/example/worker.
    /// Slash-qualified and dotted identities are exact; a simple name becomes agent/HOST.NAME.
    /// A doubled agent/agent/ prefix is rejected.
    #[arg(value_parser = parse_agent_start_identity)]
    identity: String,
    /// Override the typed harness; new seats default to claude.
    #[arg(long, value_parser = ["claude", "codex", "pi", "omp", "opencode"])]
    harness: Option<String>,
    #[arg(long)]
    host: Option<String>,
    /// Override the workspace; new seats default to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Set the typed-harness model; refused for existing command/argv seats.
    #[arg(long)]
    model: Option<String>,
    /// Set the typed-harness reasoning effort; refused for existing command/argv seats.
    #[arg(long)]
    effort: Option<String>,
    /// Replace typed-harness extra arguments; refused for existing command/argv seats.
    #[arg(long = "arg")]
    arguments: Vec<String>,
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// Read the daemon's declaration and print the effective seat KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct AgentNewArgs {
    /// Start the harness with this first message using its native prompt argument.
    #[arg(long, allow_hyphen_values = true)]
    message: Option<String>,
    /// Stable seat identity. A slash-qualified identity is kept exactly after `agent/`; a simple
    /// name is prefixed with its host, as in `agent/HOST.NAME`.
    name: String,
    /// Fleet host that runs the agent; defaults to this host.
    #[arg(long)]
    host: Option<String>,
    #[arg(long, default_value = "claude", value_parser = ["claude", "codex", "pi", "omp", "opencode"])]
    harness: String,
    /// Model the harness runs, such as `claude-opus-5-5`; defaults to the harness's own.
    #[arg(long)]
    model: Option<String>,
    /// Reasoning effort the harness runs with, such as `high`.
    #[arg(long)]
    effort: Option<String>,
    /// Directory the agent works in on its host; the host creates it when it is missing. Defaults
    /// to a new directory for the agent below that host's home, `~/st/agents/NAME`.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// What the agent is for.
    #[arg(long)]
    description: Option<String>,
    /// Attach this terminal to the agent once it is ready. Detach with Ctrl+\.
    #[arg(long, conflicts_with = "print_kdl")]
    attach: bool,
    /// Print the declaration without applying it.
    #[arg(long)]
    print_kdl: bool,
    /// How long to wait for the harness to become ready.
    #[arg(long, default_value = "10m")]
    timeout: String,
    /// Person or agent publishing the declaration; defaults to `person` in the st config.
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: Option<String>,
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
struct AgentSuspendArgs {
    /// Exact seat subject or its identity without the `agent/` prefix.
    #[arg(value_parser = parse_agent_start_identity)]
    subject: String,
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// Why the seat is suspended, recorded with the request.
    #[arg(long)]
    reason: Option<String>,
    /// How long to wait for the seat to suspend.
    #[arg(long, default_value = "5m")]
    timeout: String,
}

#[derive(Args)]
struct AgentResumeArgs {
    /// Exact seat subject or its identity without the `agent/` prefix.
    #[arg(value_parser = parse_agent_start_identity)]
    subject: String,
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// How long to wait for the seat to resume its native session.
    #[arg(long, default_value = "10m")]
    timeout: String,
}

#[derive(Args)]
struct AgentRestartArgs {
    /// Exact seat subject or its identity without the `agent/` prefix.
    #[arg(value_parser = parse_agent_start_identity)]
    subject: String,
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// How long to wait for a new running incarnation.
    #[arg(long, default_value = "10m")]
    timeout: String,
}

#[derive(Subcommand)]
enum GhCommand {
    /// Watch an issue or pull request. Each new comment or review that this seat did not post,
    /// each time the required checks on its current head turn pass or fail, and its close or
    /// merge wake this seat once; the close or merge, or the deadline, ends the watch. Watching
    /// it again keeps the watch and takes the new deadline.
    #[command(
        after_help = "Examples:\n  st gh watch acme/garden#12\n  st gh watch https://github.com/acme/garden/pull/12 --until 4h"
    )]
    Watch(GhWatchArgs),
    /// End this seat's watch on an issue or pull request.
    Unwatch(GhUnwatchArgs),
    /// List this seat's watches, running and ended in the last day, or every seat's with --all.
    Ls(GhLsArgs),
    /// Post a comment, or a pull request review, as this seat. st records the new comment's
    /// GitHub ID as this seat's, so it wakes no watch of this seat and other seats' wakes name
    /// this seat; it also watches the thread unless --no-watch.
    #[command(
        after_help = "Examples:\n  st gh comment acme/garden#12 --body 'The seed list is ready.'\n  st gh comment acme/garden#12 --body-file review.md --review request-changes"
    )]
    Comment(GhCommentArgs),
    /// Record a comment or review this seat posted some other way, by its URL, as this seat's.
    /// A watch that already reported it woke the seat once.
    Own(GhOwnArgs),
}

#[derive(Args)]
struct GhCommentArgs {
    /// OWNER/REPO#NUMBER, or the issue or pull request URL.
    thread: String,
    /// The comment's text.
    #[arg(
        long,
        conflicts_with = "body_file",
        required_unless_present = "body_file"
    )]
    body: Option<String>,
    /// Read the comment's text from a file, or `-` for standard input.
    #[arg(long)]
    body_file: Option<PathBuf>,
    /// Post a pull request review instead: approve, request-changes or comment.
    #[arg(long)]
    review: Option<String>,
    /// Post without watching the thread.
    #[arg(long)]
    no_watch: bool,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
}

#[derive(Args)]
struct GhOwnArgs {
    /// The comment's or review's URL: …#issuecomment-ID or …#pullrequestreview-ID.
    url: String,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
}

#[derive(Args)]
struct GhWatchArgs {
    /// OWNER/REPO#NUMBER, or the issue or pull request URL.
    thread: String,
    /// End the watch at this time: a duration such as 4h, or an RFC 3339 time.
    #[arg(long)]
    until: Option<String>,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
}

#[derive(Args)]
struct GhUnwatchArgs {
    /// OWNER/REPO#NUMBER, or the issue or pull request URL.
    thread: String,
    /// The seat whose watch a person ends; a seat ends its own.
    #[arg(long)]
    agent: Option<String>,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
}

#[derive(Args)]
struct GhLsArgs {
    /// Every seat's watches, not only this seat's.
    #[arg(long)]
    all: bool,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: Option<String>,
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
    /// Return the same logical result when this key is retried. A member refuses the key for a
    /// different request once it holds the first; members apart during a partition can each
    /// accept it, and then both claims stand and `st doctor` names them.
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
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
    #[arg(long)]
    reason: String,
}

#[derive(Subcommand)]
enum AttentionCommand {
    /// List all current human attention items.
    Ls {
        /// Follow current collection changes.
        #[arg(long, conflicts_with_all = ["all", "cursor"])]
        watch: bool,
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
    /// Legacy mutation: returns attention-migrated. Use work ask or remedy the source.
    Request(AttentionRequestArgs),
    /// Legacy mutation: returns attention-migrated. Complete a person step with work done.
    Resolve(AttentionResolveArgs),
    /// Legacy mutation: returns attention-migrated. Cancel your ask with work cancel-ask.
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
    /// Resolve the item on its own once this step ends, instead of the step you have claimed.
    #[arg(long, value_name = "STEP_RUN")]
    step: Option<String>,
    /// Only a person closes the item, because nothing st observes can say it is done.
    #[arg(long, conflicts_with_all = ["until", "step"])]
    person_closes: bool,
}

#[derive(Args)]
struct AttentionResolveArgs {
    subject: String,
    #[arg(long, value_parser = ["resolved", "dismissed"])]
    outcome: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long = "as", value_parser = parse_actor_subject)]
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
    /// Ask a person through a runtime step owned by live work.
    ///
    /// Puts a structured request on the person's home.
    Ask(WorkAskArgs),
    /// Bring a person information they asked for. Nothing waits on it; it clears once read.
    Update(WorkUpdateArgs),
    /// Complete a person-assigned step with a response.
    Done(WorkDoneArgs),
    /// Cancel your own ask and resume its live origin.
    CancelAsk(WorkDoneArgs),
    /// List current actionable work; use --as to filter one agent or --all for history.
    Ls {
        /// Follow current collection changes.
        #[arg(long, conflicts_with_all = ["all", "cursor", "since", "until", "status"])]
        watch: bool,
        #[arg(long = "as")]
        actor: Option<String>,
        #[arg(long)]
        all: bool,
        /// Resume the next bounded page returned by an earlier list.
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Terminal transitions since a duration such as 6h, or an RFC3339 timestamp.
        #[arg(long)]
        since: Option<String>,
        /// End of the time window (RFC3339 timestamp or duration ago).
        #[arg(long)]
        until: Option<String>,
        /// Terminal state: failed, cancelled, timed-out, or completed. Includes reasons.
        #[arg(long, value_parser = ["failed", "cancelled", "timed-out", "completed"])]
        status: Option<String>,
    },
    /// Explain one work item, its owner, readiness, lease, and evidence.
    Show { subject: String },
    /// Acquire one ready work item with the current harness incarnation.
    Claim(WorkActionArgs),
    /// Extend the live lease for work this incarnation still owns.
    Renew(WorkActionArgs),
    /// Record a material progress update without changing ownership.
    ///
    /// Records progress in the graph at no cost to anyone; people read it in stui.
    Progress(WorkActionArgs),
    /// Add time to the execution budget of claimed work that ran out of it.
    Extend(WorkExtendArgs),
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
    ///
    /// Goals, constraints and named documents encode every known rule and decision.
    /// `depends-on` orders steps; `missions start --after` orders runs without reports.
    /// A final step assigned to the author, depending on the last real step, reaches the
    /// author once when work is done. Review gates mark decisions only a person can make.
    Revise(WorkReviseArgs),
    /// Inspect or decide one mission revision proposal and its generations.
    Revision {
        #[command(subcommand)]
        command: WorkRevisionCommand,
    },
}

#[derive(Args)]
struct WorkAskArgs {
    #[arg(long = "for")]
    person: String,
    #[arg(long)]
    title: String,
    /// What the person reads. With --request it defaults to the request's question.
    #[arg(long, required_unless_present = "request")]
    reason: Option<String>,
    /// A structured request as a JSON file (`-` reads stdin): a decision, a choice or
    /// feedback, with named answers the person picks and you read back as data.
    #[arg(long, value_name = "FILE", long_help = STRUCTURED_REQUEST_HELP)]
    request: Option<PathBuf>,
    #[arg(long, conflicts_with = "new_run")]
    step: Option<String>,
    #[arg(long, conflicts_with = "step")]
    new_run: Option<String>,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
    #[arg(long, env = "ST3_INCARNATION")]
    incarnation: Option<String>,
    #[arg(long)]
    idempotency_key: String,
}

#[derive(Args)]
struct WorkUpdateArgs {
    #[arg(long = "for")]
    person: String,
    /// Where the person asked for this: their own mission run or step run, or their message to
    /// you. An update about anything else is refused.
    #[arg(long, value_name = "RUN|STEP|MESSAGE")]
    about: String,
    #[arg(long)]
    title: String,
    /// The information itself.
    #[arg(long)]
    body: String,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
    #[arg(long)]
    idempotency_key: String,
}

#[derive(Args)]
struct WorkDoneArgs {
    subject: String,
    #[arg(long = "as")]
    actor: String,
    /// The response in words. A structured answer derives it when omitted.
    #[arg(long, alias = "reason", required_unless_present_any = ["answer", "text"])]
    summary: Option<String>,
    /// The ID of one of a structured request's named answers.
    #[arg(long, value_name = "ID")]
    answer: Option<String>,
    /// Text for a structured request: feedback, a custom choice, or the changes requested.
    #[arg(long)]
    text: Option<String>,
    #[arg(long)]
    evidence: Vec<String>,
    #[arg(long)]
    episode: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
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
struct WorkExtendArgs {
    subject: String,
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long, env = "ST3_INCARNATION")]
    incarnation: Option<String>,
    /// Time to add to this attempt's budget, such as 30m or 2h; at most 7d.
    #[arg(long)]
    by: String,
    /// Why the step needs more time.
    #[arg(long)]
    reason: String,
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
    /// Search readable messages and normalized agent transcripts, newest first.
    Search {
        text: String,
        #[arg(long = "as", value_parser = parse_actor_subject)]
        actor: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        /// Include entries at or after this RFC3339 timestamp.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Send one durable normalized message to a person or agent.
    ///
    /// A message is a direct connection: it wakes the recipient agent for a full turn,
    /// which rereads its context.
    Send(MessageSendArgs),
    /// List the current mailbox for one explicit identity.
    Ls(MessageListArgs),
    /// Show delivery and read progress without changing the message lifecycle, or whether an
    /// unconfirmed send landed.
    Status(MessageStatusArgs),
    /// Read exact messages and optionally mark them read or archived.
    Read(MessageReadArgs),
    /// Reply to one canonical message ID while preserving its thread.
    ///
    /// A message is a direct connection: it wakes the recipient agent for a full turn,
    /// which rereads its context.
    Reply(MessageReplyArgs),
    /// Close exact messages after their related action is complete.
    Archive(MessageArchiveArgs),
    /// Render the bounded conversation thread around one message.
    Thread(MessageReferenceArgs),
    /// List normalized harness sessions available for native conversation views.
    Sessions {
        #[arg(long = "as", value_parser = parse_actor_subject)]
        actor: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show one conversation as stui shows it: messages, folded tools, Small Talk. `--raw`
    /// prints every normalized entry (ids, message boundaries, tool JSON); `--json` the page.
    Timeline {
        session: String,
        #[arg(long = "as", value_parser = parse_actor_subject)]
        actor: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Continue toward older entries using the preceding response's next cursor.
        #[arg(long)]
        cursor: Option<String>,
        /// Every normalized entry as it is stored, instead of the conversation as it reads.
        #[arg(long)]
        raw: bool,
        /// Simplified: a tool call to a line, and a run of calls to one line.
        #[arg(long, conflicts_with = "raw")]
        simple: bool,
    },
    /// Follow the visible normalized conversation; JSON output is one entry per line.
    Follow {
        session: String,
        #[arg(long = "as", value_parser = parse_actor_subject)]
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
    /// Attach an image (PNG, JPEG, GIF or WebP, at most 10 MiB, up to 4). It is uploaded to this
    /// member and fetched by the machine that reads or delivers the message.
    #[arg(long = "attach", value_name = "FILE")]
    attach: Vec<PathBuf>,
    /// Print the generated message mission KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
    /// Name this message for retries. Without it the key comes from the sender, recipient, words
    /// and attachments, so running the same command again within the hour (or the next) reports
    /// the message already sent instead of sending it twice.
    #[arg(long)]
    idempotency_key: Option<String>,
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
    /// Attach an image (PNG, JPEG, GIF or WebP, at most 10 MiB, up to 4).
    #[arg(long = "attach", value_name = "FILE")]
    attach: Vec<PathBuf>,
    /// Print the generated reply mission KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
    /// Name this reply for retries. Without it the key comes from the sender, the message
    /// replied to, the words and attachments, as for `send`.
    #[arg(long)]
    idempotency_key: Option<String>,
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
struct MessageStatusArgs {
    #[arg(required_unless_present = "idempotency_key")]
    reference: Option<String>,
    /// Whether a send or reply that st did not confirm landed, by the key its error printed.
    #[arg(long, conflicts_with = "reference")]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct ReviewArgs {
    /// The gate to answer: its `attention/...` ID from `st attention ls`, or the step, mission
    /// or loop run (`step-run/...`, `mission-run/...`, `loop-run/...`) that owns it.
    target: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args)]
struct FeedbackReviewArgs {
    /// The feedback gate to answer: its `attention/...` ID from `st attention ls`, or the
    /// step run that owns it.
    target: String,
    #[arg(long)]
    reason: String,
    #[arg(long = "as", value_parser = parse_actor_subject)]
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
    #[arg(long, requires = "initial_message_id", allow_hyphen_values = true)]
    initial_message: Option<String>,
    #[arg(long, requires = "initial_message")]
    initial_message_id: Option<String>,
    #[arg(last = true)]
    argv: Vec<String>,
}

#[derive(Args)]
struct SkillArgs {
    #[command(subcommand)]
    command: Option<SkillCommand>,
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Write the skill where each named harness loads user skills; no name installs it for all.
    Install {
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(st3::skill::HARNESSES))]
        harness: Vec<String>,
    },
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

fn main() -> ExitCode {
    // A recorder link starts st3 as `git` or `gh`. It must not build the async runtime.
    if let Some(program) = st3::recorder::invoked_program() {
        st3::recorder::run(program);
    }
    // A seat process asks a replacement binary which resume formats it reads before executing it.
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(st_drivers::reexec::PROBE_SUBCOMMAND))
    {
        println!("{}", st_drivers::reexec::probe_answer());
        return ExitCode::SUCCESS;
    }
    // A seat's harness runs its hooks through this binary. They are hidden from help, need no
    // daemon or config to start, and the status line runs every few seconds, so they skip the
    // CLI parser and the async runtime.
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(st3::driver_hook::SUBCOMMAND))
    {
        return run_driver_hook();
    }
    // SAFETY: no other thread exists yet; the async runtime starts after this returns.
    unsafe { st_drivers::reexec::take_resume_environment() };
    if st_drivers::reexec::resume_path(st_drivers::reexec::DRIVER_RESUME_ENV).is_some() {
        // The predecessor blocked the stop signals across its exec. Install the handlers before
        // unblocking them, so a stop that arrived in between ends the session the ordinary way.
        st_drivers::provider_session::install_stop_handlers();
        st_drivers::reexec::unblock_stop_signals();
    }
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if cli_help::all_help_requested(&arguments) {
        print!("{}", cli_help::root_help(true));
        return ExitCode::SUCCESS;
    }
    let matches = Cli::command()
        .override_help(cli_help::root_help(false))
        .try_get_matches_from(arguments)
        .unwrap_or_else(|error| exit_usage_error(error));
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| exit_usage_error(error));
    if let Command::Up(args) = &cli.command {
        record_daemon_commands(args);
    }
    run_cli(cli)
}

/// Print a usage error and exit. Inside a gate check the refusal also reaches the gate's
/// report, so a pipeline that hides st's exit status cannot hide the refusal.
fn exit_usage_error(error: clap::Error) -> ! {
    if !matches!(
        error.kind(),
        clap::error::ErrorKind::DisplayHelp
            | clap::error::ErrorKind::DisplayVersion
            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        let reason = error
            .kind()
            .as_str()
            .unwrap_or("the arguments are not valid");
        st3::gate_report::note_refusal(reason);
    }
    error.exit()
}

/// `st driver-hook NAME [ARGS]`: answer one harness hook with the payload on stdin.
fn run_driver_hook() -> ExitCode {
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
    let run = || st3::driver_hook::run(
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
    );
    let code = if name == "claude-observe" {
        st3::telemetry::hook(&env, run)
    } else {
        st3::telemetry::local_only();
        run()
    };
    ExitCode::from(code)
}

/// The daemon finds `git` and `gh` through its recorder like every member does. `run_up` installs
/// the directory; until then PATH lookups skip it.
fn record_daemon_commands(args: &UpArgs) {
    let state_dir = match &args.state_dir {
        Some(state_dir) => state_dir.clone(),
        None => match Config::load_unvalidated(args.config.as_deref()) {
            Ok(config) => config.state_dir,
            // `run_up` reports the configuration error.
            Err(_) => return,
        },
    };
    let Ok(directory) = st3::recorder::directory(&state_dir) else {
        return;
    };
    let Ok(path) = st3::recorder::prepend(&directory, std::env::var_os("PATH").as_deref()) else {
        return;
    };
    // SAFETY: no other thread exists yet; the async runtime starts after this returns.
    unsafe { std::env::set_var("PATH", path) };
}

#[tokio::main]
async fn run_cli(cli: Cli) -> ExitCode {
    if matches!(&cli.command, Command::Driver(_)) {
        st3::telemetry::local_only();
    }
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<CommandExit>() {
                return ExitCode::from(exit.0);
            }
            eprintln!("st: {}", plain_error(&error));
            if refused_command(&error) {
                st3::gate_report::note_refusal(&plain_error(&error));
            }
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

/// The error chain as printed, with an st client-API error said in plain words and its code in
/// parentheses for scripts, rather than a Rust enum name (`StaleFence`).
fn plain_error(error: &anyhow::Error) -> String {
    let full = format!("{error:#}");
    let Some(api) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<st3_client::ClientError>())
    else {
        return full;
    };
    let st3_client::ClientError::Api(code, _, _) = api else {
        return full;
    };
    let code = serde_json::to_value(code)
        .ok()
        .and_then(|code| code.as_str().map(str::to_owned))
        .unwrap_or_default();
    full.replace(&api.to_string(), &format!("{} ({code})", api.plain()))
}

static DAEMON_WAIT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// A command client that waits out a daemon restart for the `--daemon-wait` window.
fn cli_client(endpoint: &Endpoint) -> Client {
    Client::new(endpoint.clone())
        .with_outage_wait(DAEMON_WAIT.get().copied().unwrap_or_default(), true)
}

/// Whether st turned the command down, as opposed to answering it. A missing subject, a stale
/// fence, a wait that timed out, an unreachable or slow daemon and a daemon error are answers or
/// passing conditions a gate may wait out; a refusal never passes, so it marks a gate broken.
fn refused_command(error: &anyhow::Error) -> bool {
    if daemon_is_unreachable(error) || st3::client::daemon_did_not_answer(error) {
        return false;
    }
    // A reader that stopped early, such as `grep -q` on a match, closed the pipe: st answered.
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::BrokenPipe)
    }) {
        return false;
    }
    let message = error.to_string();
    if message.contains("stale-subject")
        || message.contains("terminal status selected")
        || message.contains("wait timed out")
    {
        return false;
    }
    if let Some(api) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<GeneratedClientError>())
    {
        return matches!(
            api,
            GeneratedClientError::Api(
                ClientErrorCode::Forbidden
                    | ClientErrorCode::UnsupportedCapability
                    | ClientErrorCode::ValidationFailed
                    | ClientErrorCode::AttentionMigrated
                    | ClientErrorCode::UnsupportedMediaType
                    | ClientErrorCode::BlobTooLarge,
                _,
                _
            )
        );
    }
    match st3::client::http_status(error) {
        Some(status) => matches!(status, 400 | 401 | 403 | 405 | 413 | 415 | 422),
        // The command refused its own arguments before it asked the daemon.
        None => true,
    }
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
    // Fabric runs this for each tunnel. It needs no config and no daemon.
    if let Command::Terminals {
        command: PtyCommand::ServeFabric(args),
    } = &cli.command
    {
        return st3::terminal_fabric::serve_stdio(&args.pty_root).await;
    }
    if let Command::Skill(args) = cli.command {
        return run_skill(args);
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
        if args.peer_listen_allow_plain_http {
            config.peer_listen_allow_plain_http = true;
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
        .unwrap_or_else(|| Endpoint::Unix(config.client_socket()));
    let _ = DAEMON_WAIT.set(Duration::from_secs(cli.daemon_wait));
    let client = cli_client(&endpoint);
    // Drivers outlive daemon restarts and handle an outage in their own loops; doctor reports one.
    let immediate = Client::new(endpoint.clone());
    match cli.command {
        Command::Apply(args) => run_owned_set_apply(&client, args, cli.json).await,
        Command::Sets { command } => run_owned_sets(&endpoint, command, cli.json).await,
        Command::Up(_) => unreachable!(),
        Command::Skill(_) => unreachable!(),
        Command::ReplicationWorker(_) => unreachable!(),
        Command::Now(args) => run_now(&endpoint, config.person.as_deref(), args, cli.json).await,
        Command::Usage(args) => run_usage(&immediate, args, cli.json).await,
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
        Command::Gh { command } => run_gh(&client, command, cli.json).await,
        Command::Lanes { command } => {
            run_lanes(&client, config.person.as_deref(), command, cli.json).await
        }
        Command::Terminals {
            command: PtyCommand::ExposeFabric(args),
        } => expose_fabric(&config, args).await,
        Command::Terminals { command } => {
            // This host's PTY root, whichever endpoint answers: a terminal whose session runs
            // there is attached through it when that endpoint cannot answer in time.
            let pty_root = config
                .pty_root
                .clone()
                .unwrap_or_else(|| config.state_dir.join("pty"));
            run_pty(
                &client,
                &endpoint,
                config.person.as_deref(),
                Some(&pty_root),
                command,
                cli.json,
            )
            .await
        }
        Command::Doctor(args) => run_doctor(&immediate, args, cli.json).await,
        Command::Recorder { command } => run_recorder(command, &config, cli.json),
        Command::Repair { command } => run_repair(&client, command, cli.json).await,
        Command::Replication { command } => {
            run_replication(&client, &config, command, cli.json).await
        }
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
        Command::Blobs { command } => {
            run_blobs(&endpoint, config.person.as_deref(), command, cli.json).await
        }
        Command::Rules { command } => run_rules(&client, &config, command, cli.json).await,
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
        Command::Gate { command } => run_gate(command).await,
    }
}

/// `st gate KIND`: print the answer and exit with the status an exec gate reads.
async fn run_gate(command: GateCommand) -> Result<()> {
    use st3::resource::github_gates;
    let answer = match command {
        GateCommand::Merged { pull_request } => {
            github_gates::pull_request_merged(&pull_request).await
        }
        GateCommand::CiPassed {
            check,
            repo,
            reference,
        } => github_gates::check_passed(&repo, &reference, &check).await,
        GateCommand::CargoTest {
            target,
            package,
            reference,
            repository,
            worktree,
        } => {
            tokio::task::spawn_blocking(move || {
                st3::gate_kinds::cargo_test(&st3::gate_kinds::CargoTest {
                    target: &target,
                    package: &package,
                    reference: &reference,
                    repository: &repository,
                    worktree: worktree.as_deref(),
                })
            })
            .await?
        }
    };
    println!("{}", answer.describe());
    match answer.exit_code() {
        0 => Ok(()),
        code => Err(CommandExit(code).into()),
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
        Command::Apply(args) => Some(args.actor.as_str()),
        Command::Missions { command } => match command {
            MissionViewCommand::Publish(args) => Some(args.actor.as_str()),
            MissionViewCommand::Start(args) => Some(args.actor.as_str()),
            MissionViewCommand::Cancel(args) => Some(args.actor.as_str()),
            MissionViewCommand::Outcome(args) => Some(args.actor.as_str()),
            MissionViewCommand::Retire(args) => Some(args.actor.as_str()),
            MissionViewCommand::Release(args) | MissionViewCommand::CancelRequest(args) => {
                Some(args.actor.as_str())
            }
            _ => None,
        },
        Command::Import {
            command: ImportCommand::Run { person, .. },
        } => Some(person.as_str()),
        Command::Terminals { command } => match command {
            PtyCommand::InputClient(args) => args.person.as_deref(),
            PtyCommand::DetachClient(args) => args.person.as_deref(),
            _ => None,
        },
        Command::Agents { command } => match command {
            AgentsCommand::New(args) => Some(args.actor.as_deref().ok_or_else(|| {
                anyhow::anyhow!("a harness `st agents new` needs explicit --as {own}; it cannot use the configured person")
            })?),
            AgentsCommand::Apply(args) => Some(args.actor.as_str()),
            AgentsCommand::Start(args) => Some(args.actor.as_str()),
            AgentsCommand::Stop(args) => Some(args.actor.as_str()),
            AgentsCommand::Restart(args) => Some(args.actor.as_str()),
            AgentsCommand::Suspend(args) => Some(args.actor.as_str()),
            AgentsCommand::Resume(args) => Some(args.actor.as_str()),
            AgentsCommand::Hold(args) if args.duration.is_some() || args.release => Some(args.actor.as_deref().ok_or_else(|| {
                anyhow::anyhow!("a harness delivery hold needs explicit --as {own}")
            })?),
            AgentsCommand::Rename(args) => Some(args.actor.as_deref().ok_or_else(|| {
                anyhow::anyhow!("a harness rename needs explicit --as {own}")
            })?),
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
        Command::Lanes { command } => match command {
            LaneCommand::Join(args) | LaneCommand::Approve(args) => args.actor.as_deref(),
            LaneCommand::Leave(args) => args.entry.actor.as_deref(),
            LaneCommand::Move(args) => args.entry.actor.as_deref(),
            LaneCommand::Mark(args) => args.actor.as_deref(),
            LaneCommand::Ls { .. } | LaneCommand::Show { .. } => None,
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
        Command::Gh { command } => match command {
            GhCommand::Watch(args) => Some(args.actor.as_str()),
            GhCommand::Unwatch(args) => Some(args.actor.as_str()),
            GhCommand::Comment(args) => Some(args.actor.as_str()),
            GhCommand::Own(args) => Some(args.actor.as_str()),
            GhCommand::Ls(_) => None,
        },
        _ => None,
    };
    if let Some(actor) = actor {
        if actor.starts_with("person/") || actor == "requester" {
            anyhow::bail!(
                "this harness is `{own}` (ST_AGENT) and cannot act as `{actor}` on a mutating command; request a person through `st work ask --as \"$ST_AGENT\"`"
            );
        }
        if let Some(message) = foreign_agent_actor(actor, Some(own), mission_run) {
            anyhow::bail!(message);
        }
    }
    Ok(())
}

/// One watch as one line: thread, state, deadline or ending, and title.
fn gh_watch_line(view: &Value, with_agent: bool) -> String {
    let text = |name: &str| view.get(name).and_then(Value::as_str).unwrap_or_default();
    let state = match text("state") {
        "ended" => format!("ended ({})", text("ended")),
        "degraded" => format!("degraded: {}", text("reason")),
        state => state.to_owned(),
    };
    let until = view
        .get("until")
        .and_then(Value::as_str)
        .map(|until| format!(" until {until}"))
        .unwrap_or_default();
    let title = view
        .get("title")
        .and_then(Value::as_str)
        .map(|title| format!(" \"{title}\""))
        .unwrap_or_default();
    let agent = if with_agent {
        format!(" {}", text("agent"))
    } else {
        String::new()
    };
    format!("{}{agent}  {state}{until}{title}", text("thread"))
}

async fn run_gh(client: &st3::client::Client, command: GhCommand, json: bool) -> Result<()> {
    match command {
        GhCommand::Watch(args) => {
            let view: Value = client
                .post(
                    "/v1/github/watch",
                    &json!({"actor": args.actor, "thread": args.thread, "until": args.until}),
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
                return Ok(());
            }
            let thread = view
                .get("thread")
                .and_then(Value::as_str)
                .unwrap_or_default();
            println!("Watching {}", gh_watch_line(&view, false));
            println!(
                "Each new comment or review that this seat did not post, each time the required checks on its head turn pass or fail, and its close or merge wake this seat once."
            );
            println!("To stop: st gh unwatch {thread}");
        }
        GhCommand::Unwatch(args) => {
            let outcome: Value = client
                .post(
                    "/v1/github/unwatch",
                    &json!({"actor": args.actor, "thread": args.thread, "agent": args.agent}),
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
                return Ok(());
            }
            let thread = outcome
                .get("thread")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let agent = outcome
                .get("agent")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if outcome.get("ended") == Some(&Value::Bool(true)) {
                println!("Ended the watch of {agent} on {thread}.");
            } else {
                println!("{agent} has no running watch on {thread}.");
            }
        }
        GhCommand::Comment(args) => {
            let body = match (args.body, args.body_file) {
                (Some(body), _) => body,
                (None, Some(path)) if path.as_os_str() == "-" => {
                    let mut body = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)?;
                    body
                }
                (None, Some(path)) => std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?,
                (None, None) => anyhow::bail!("give the comment's text with --body or --body-file"),
            };
            let posted: Value = client
                .post(
                    "/v1/github/comment",
                    &json!({"actor": args.actor, "thread": args.thread, "body": body,
                        "review": args.review, "watch": !args.no_watch}),
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&posted)?);
                return Ok(());
            }
            let text = |name: &str| posted.get(name).and_then(Value::as_str).unwrap_or_default();
            println!(
                "Posted {} {} as this seat's: {}",
                text("kind"),
                posted["id"],
                text("url")
            );
            if let Some(watch) = posted.get("watch").filter(|watch| !watch.is_null()) {
                println!("Watching {}", gh_watch_line(watch, false));
            }
        }
        GhCommand::Own(args) => {
            let recorded: Value = client
                .post(
                    "/v1/github/own",
                    &json!({"actor": args.actor, "url": args.url}),
                )
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&recorded)?);
                return Ok(());
            }
            println!(
                "Recorded {} {} on {} as this seat's.",
                recorded["kind"].as_str().unwrap_or_default(),
                recorded["id"],
                recorded["thread"].as_str().unwrap_or_default()
            );
        }
        GhCommand::Ls(args) => {
            let agent = if args.all {
                None
            } else {
                Some(args.actor.clone().context(
                    "outside an agent seat, list every seat's watches with --all or name one with --as",
                )?)
            };
            let path = agent.as_deref().map_or_else(
                || "/v1/github/watches".to_owned(),
                |agent| format!("/v1/github/watches?agent={}", urlencoding::encode(agent)),
            );
            let views: Vec<Value> = client.get(&path).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&views)?);
                return Ok(());
            }
            if views.is_empty() {
                println!("No watches.");
            }
            for view in &views {
                println!("{}", gh_watch_line(view, agent.is_none()));
            }
        }
    }
    Ok(())
}

fn run_skill(args: SkillArgs) -> Result<()> {
    let Some(SkillCommand::Install { harness }) = args.command else {
        print!("{}", st3::skill::SKILL);
        return Ok(());
    };
    let harnesses = if harness.is_empty() {
        st3::skill::HARNESSES.map(str::to_owned).to_vec()
    } else {
        harness
    };
    let mut installed = BTreeSet::new();
    for harness in &harnesses {
        let path = st3::skill::install(harness)?;
        if installed.insert(path.clone()) {
            println!("{}", path.display());
        }
    }
    Ok(())
}

fn run_claude_channel(command: ClaudeChannelCommand) -> Result<()> {
    match command {
        ClaudeChannelCommand::Install { no_policy } => {
            st_drivers::claude_channel::install_st3(no_policy).map(|_| ())
        }
        ClaudeChannelCommand::Status => st_drivers::claude_channel::status_st3(),
        ClaudeChannelCommand::Uninstall { keep_policy } => {
            st_drivers::claude_channel::uninstall_st3(keep_policy)
        }
        ClaudeChannelCommand::InstallPolicy => {
            st_drivers::claude_channel::install_st3_policy().map(|_| ())
        }
        ClaudeChannelCommand::UninstallPolicy => st_drivers::claude_channel::uninstall_st3_policy(),
    }
}

fn run_recorder(command: RecorderCommand, config: &Config, json_output: bool) -> Result<()> {
    match command {
        RecorderCommand::Report(args) => {
            anyhow::ensure!(args.hours > 0, "--hours must be greater than zero");
            let hours = i64::try_from(args.hours).context("--hours is too large")?;
            let window = chrono::Duration::try_hours(hours).context("--hours is too large")?;
            let until = chrono::Utc::now();
            let since = until
                .checked_sub_signed(window)
                .context("--hours is too large")?;
            let logs = if args.logs.is_empty() {
                let local = st3::recorder::log_path(&config.state_dir)?;
                if local.exists() {
                    vec![local]
                } else {
                    Vec::new()
                }
            } else {
                args.logs
            };
            let report = st3::recorder_report::summarize(logs, since, until, args.top)?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", st3::recorder_report::render(&report));
            }
            Ok(())
        }
    }
}

/// Every read that runs at once has a SQLite connection of its own, a few open files each, and
/// every seat and client holds a socket. Raise the soft open file limit, which is often 1024,
/// toward the hard one, so a burst of requests cannot run out of descriptors. Children inherit
/// it, so it stays modest.
fn raise_open_file_limit() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit fills the rlimit it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }
    let wanted = limit.rlim_max.min(8_192);
    if wanted <= limit.rlim_cur {
        return;
    }
    let raised = libc::rlimit {
        rlim_cur: wanted,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: setrlimit only reads the rlimit it is given.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } != 0 {
        eprintln!(
            "st3: could not raise the open file limit from {}: {}",
            limit.rlim_cur,
            std::io::Error::last_os_error()
        );
    }
}

fn select_private_gateway(config: &mut Config, private_state: bool, private_socket: bool) {
    if !(private_state || private_socket) {
        return;
    }
    let defaults = Config::default();
    if config.state_dir == defaults.state_dir && config.socket == defaults.socket {
        return;
    }
    let parent = if private_socket {
        config
            .socket
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
    } else {
        Some(config.state_dir.as_path())
    };
    config.client_gateway_socket = parent
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("st3-client.sock");
}

#[cfg(test)]
mod private_gateway_tests {
    use super::*;

    #[test]
    fn private_state_and_socket_derive_a_private_gateway() {
        let mut config = Config::default();
        let default_gateway = config.client_gateway_socket.clone();
        config.state_dir = "/tmp/private-state".into();
        select_private_gateway(&mut config, true, false);
        assert_eq!(
            config.client_gateway_socket,
            PathBuf::from("/tmp/private-state/st3-client.sock")
        );
        config.socket = "/tmp/private-socket/api.sock".into();
        select_private_gateway(&mut config, true, true);
        assert_eq!(
            config.client_gateway_socket,
            PathBuf::from("/tmp/private-socket/st3-client.sock")
        );
        assert_ne!(config.client_gateway_socket, default_gateway);
    }

    #[test]
    fn default_daemon_keeps_its_default_gateway() {
        let mut config = Config::default();
        let gateway = config.client_gateway_socket.clone();
        select_private_gateway(&mut config, false, false);
        assert_eq!(config.client_gateway_socket, gateway);
    }
}

async fn run_up(args: UpArgs) -> Result<()> {
    let private_state = args.state_dir.is_some();
    let private_socket = args.socket.is_some();
    let explicit_gateway = args.client_gateway_socket.is_some();
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
    if !explicit_gateway {
        select_private_gateway(&mut config, private_state, private_socket);
    }
    if let Some(peer_listen) = args.peer_listen {
        config.peer_listen = Some(peer_listen);
    }
    if args.peer_listen_allow_plain_http {
        config.peer_listen_allow_plain_http = true;
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
    validate_unix_socket_path(&config.socket, "--socket")?;
    validate_unix_socket_path(&config.client_gateway_socket, "--client-gateway-socket")?;
    fs::create_dir_all(&config.state_dir)?;
    st3::hooks::ensure_installed(&st3::hooks::root(&config.state_dir)).context(
        "publishing this st binary's required lifecycle hook set before starting the daemon",
    )?;
    st3::profile::init_from_env();
    raise_open_file_limit();
    let store = Arc::new(st3::profile::task("startup open-store", || {
        Store::open(&config.state_dir.join("claims.sqlite3"), &config.node)
    })?);
    if let Some(fleet_id) = &config.fleet_id {
        store.bind_fleet(fleet_id)?;
    }
    // A member pins its anchor, applies its writer floor, and signs with its key before it
    // writes anything, so every local batch after this point is signed.
    st3::fleet::activate(&store, &config)?;
    // Every claim this node writes is signed; keys are made here the first time, silently.
    let keys = st3::fleet::join::key_directory(&config.state_dir);
    if config.fleet.is_none() {
        store.set_node_key(Arc::new(st3::fleet::join::standalone_node_key(
            &config.state_dir,
        )?))?;
    }
    store.use_key_directory(&keys)?;
    st3::profile::task("startup judge-claims", || store.judge_claims(true))?;
    let admission = st3::profile::task("startup validate-replication-backlog", || {
        store.validate_replication_backlog()
    })?;
    st3::profile::task("startup apply-replication-repairs", || {
        store.apply_replication_repairs()
    })?;
    for run in st3::profile::task("startup settle-runs", || {
        store.settle_runs_for_canonical_replay()
    })? {
        eprintln!("st: mission run `{run}` stays over as this node's graph showed it");
    }
    let projected = st3::profile::task("startup project-replication-backlog", || {
        store.project_replication_backlog()
    })?;
    if !projected {
        eprintln!(
            "st: the replicated projection is stale; the daemon will use its last good graph"
        );
    }
    if admission.unknown != 0 {
        eprintln!("st: {} claims waiting for a newer build", admission.unknown);
    }
    if admission.invalid != 0 {
        eprintln!("st: replication has {} invalid records", admission.invalid);
    }
    store.append_claim(&ClaimInput {
        subject: format!("daemon/{}", config.node),
        kind: "daemon.started".into(),
        actor: None,
        fields: BTreeMap::from([
            ("features".into(), serde_json::json!({"owned_sets":1})),
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
    let login_environment = st3::environment::snapshot_at_startup()?;
    st_runtime::initialize_isolation(&login_environment);
    let pty_binary = match args.pty_binary.clone() {
        Some(pty_binary) => pty_binary,
        None => st_runtime::resolve_executable("pty", &login_environment)?,
    };
    let recorder = install_recorder(&config, &login_environment);
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
        client_relay: st3::peer::ClientRelay::from_config(&config)?
            .map(|relay| relay.with_links(store.clone())),
        native_session_home: std::env::var_os("HOME").map(PathBuf::from),
        planner_default: config.planner.clone(),
    };
    st3::gate_check::set_endpoint(config.socket.display().to_string());
    let reconciler = Arc::new(Reconciler::native(
        store.clone(),
        &config.state_dir,
        Some(&pty_root),
        &pty_binary,
        config.node.clone(),
        config.socket.display().to_string(),
        notify.clone(),
        event_notify.clone(),
        recorder.map(|installation| installation.directory),
    )?);
    tokio::spawn(reconciler.supervise());
    // A start no longer rebuilds the operation projection; check it once the API serves.
    tokio::spawn({
        let store = store.clone();
        async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            match tokio::task::spawn_blocking(move || store.repair_operation_projection_drift())
                .await
            {
                Ok(Ok(true)) => eprintln!("st3: rebuilt an operation projection that drifted"),
                Ok(Ok(false)) => {}
                Ok(Err(error)) => eprintln!("st3: operation projection check failed: {error:#}"),
                Err(error) => eprintln!("st3: operation projection check stopped: {error}"),
            }
        }
    });
    tokio::spawn(st3::profile::watch_runtime_lag());
    if config.limits.enabled {
        tokio::spawn(enforce_account_limits(
            store.clone(),
            st3::store::LimitsPolicy {
                stop_at_weekly_percent: config.limits.stop_at_weekly_percent,
                keep: config.limits.keep.iter().cloned().collect(),
                notify: config
                    .limits
                    .notify
                    .clone()
                    .expect("the daemon validated its limits operations agent"),
                fresh_ms: config
                    .limits
                    .fresh_ms()
                    .expect("the daemon validated its limits freshness"),
            },
        ));
    }
    tokio::spawn(trim_local_observations(
        store.clone(),
        config.observations.clone(),
    ));
    if config.checkpoint.enabled {
        tokio::spawn(run_checkpoints(
            store.clone(),
            st3::store::CheckpointContext {
                now_unix_ms: 0,
                configured_peers: config.peers.iter().map(|peer| peer.name.clone()).collect(),
                scratch: config.state_dir.join("checkpoint"),
                reviewer: config
                    .person
                    .clone()
                    .unwrap_or_else(|| "person/operator".into()),
            },
        ));
    }
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
    let state_socket = config.state_dir.join("run/st3.sock");
    let client_gateway_socket = config.client_gateway_socket.clone();
    // The first diagnostic report reads the whole claim log; no read waits for it.
    st3::api::start_operation_report(&state);
    // Nor does the first session list wait to read every native transcript's header.
    st3::api::start_native_session_discovery(&state);
    tokio::try_join!(
        st3::api::serve_unix_bound(&local_socket, &state_socket, router(state.clone())),
        serve_unix(&client_gateway_socket, fabric_router(state)),
    )?;
    Ok(())
}

/// Links `git` and `gh` to this executable for the daemon and its members. The daemon still
/// starts when it cannot; it then says so, and nothing is recorded.
fn install_recorder(
    config: &Config,
    login_environment: &BTreeMap<String, String>,
) -> Option<st3::recorder::Installation> {
    let daemon_path = std::env::var_os("PATH").unwrap_or_default();
    let login_path = login_environment
        .get("PATH")
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    let installed = std::env::current_exe()
        .context("find the st3 executable")
        .and_then(|executable| {
            st3::recorder::install(
                &config.state_dir,
                &config.node,
                &executable,
                &[&daemon_path, &login_path],
            )
        });
    match installed {
        Ok(installation) if installation.programs.is_empty() => {
            eprintln!("st: neither git nor gh is on PATH, so no calls are recorded");
            Some(installation)
        }
        Ok(installation) => {
            eprintln!(
                "st: recording {} calls in {}",
                installation.programs.join(" and "),
                installation.log.display()
            );
            Some(installation)
        }
        Err(error) => {
            eprintln!("st: not recording git and gh calls: {error:#}");
            None
        }
    }
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
                return follow_mission_run(client, run, 0, json_output).await;
            }
            if json_output {
                return print_value(&run, true);
            }
            let runs = load_mission_run_tree(client, &run).await?;
            let now = current_unix_ms()?;
            print!(
                "{}",
                render_mission_run(&run, &runs, OutputStyle::stdout(), now)
            );
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
        MissionViewCommand::Publish(args) => publish_mission_file(client, args, json_output).await,
        MissionViewCommand::Check(args) => check_mission_file(client, args, json_output).await,
        MissionViewCommand::Start(args) => start_mission_run(client, args, json_output).await,
        MissionViewCommand::Cancel(args) => {
            cancel_mission_run(client, endpoint, args, json_output).await
        }
        MissionViewCommand::Outcome(args) => {
            set_mission_run_outcome(client, args, json_output).await
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
    warn_ignored_authority(&mission);
    if !args.no_gate_check {
        check_before_publish(client, &intent, &args.workspace).await?;
    }
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

/// Run each exec gate once before a publication and refuse it when one is broken.
async fn check_before_publish(
    client: &Client,
    intent: &IntentInput,
    workspace: &Path,
) -> Result<()> {
    let mut announced = false;
    let checked = run_gate_check(client, intent, workspace, &[], |view, item| {
        if !announced {
            eprintln!("{}", gate_check_heading(view));
            announced = true;
        }
        eprint!("{}", render_gate_check_item(item));
    })
    .await?;
    let Some(view) = checked else {
        eprintln!("st: this st daemon cannot check exec gates; publishing without the check");
        return Ok(());
    };
    let broken = view
        .gates
        .iter()
        .filter(|item| item.answer == "broken")
        .collect::<Vec<_>>();
    anyhow::ensure!(
        broken.is_empty(),
        "{} exec gate{} cannot answer as written, so st did not publish: {}. Correct {}, or publish with --no-gate-check",
        broken.len(),
        if broken.len() == 1 { "" } else { "s" },
        broken
            .iter()
            .map(|item| format!(
                "`{}` ({})",
                item.gate,
                item.reason.as_deref().unwrap_or("broken")
            ))
            .collect::<Vec<_>>()
            .join("; "),
        if broken.len() == 1 { "it" } else { "them" }
    );
    Ok(())
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

/// Run each exec gate of `intent` once on the daemon and wait for every answer, handing each one
/// to `report` as it arrives. `None` when the daemon predates gate checks.
async fn run_gate_check(
    client: &Client,
    intent: &IntentInput,
    workspace: &Path,
    inputs: &[(String, String)],
    mut report: impl FnMut(&st3::model::GateCheckView, &st3::model::GateCheckItemView),
) -> Result<Option<st3::model::GateCheckView>> {
    let workspace = std::path::absolute(workspace)
        .with_context(|| format!("resolve the workspace {}", workspace.display()))?;
    let request = st3::model::GateCheckRequest {
        intent: intent.clone(),
        workspace: workspace.display().to_string(),
        inputs: inputs.iter().cloned().collect(),
    };
    let mut view: st3::model::GateCheckView = match client.post("/v1/gate-checks", &request).await {
        Ok(view) => view,
        Err(error) if matches!(st3::client::http_status(&error), Some(404 | 405)) => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let mut reported = 0;
    loop {
        while let Some(item) = view.gates.get(reported) {
            if matches!(item.answer.as_str(), "waiting" | "running") {
                break;
            }
            report(&view, item);
            reported += 1;
        }
        if view.finished {
            return Ok(Some(view));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        view = client
            .get(&format!(
                "/v1/gate-checks/{}",
                urlencoding::encode(&view.id)
            ))
            .await?;
    }
}

fn gate_check_heading(view: &st3::model::GateCheckView) -> String {
    format!(
        "Checking {} exec gate{} on {}, one at a time:",
        view.gates.len(),
        if view.gates.len() == 1 { "" } else { "s" },
        view.host
    )
}

/// One gate's answer: its label, owner and name, why it is broken or unchecked, and the end of a
/// broken check's output.
fn render_gate_check_item(item: &st3::model::GateCheckItemView) -> String {
    use std::fmt::Write as _;
    let label = match item.answer.as_str() {
        "not-yet" => "not yet",
        answer => answer,
    };
    let mut output = format!("  {label:<9}  {} · {}", item.owner, item.gate);
    if let Some(code) = item.exit_code {
        let _ = write!(output, " · exit {code}");
    }
    if item.elapsed_ms >= 1_000 {
        let _ = write!(output, " · {:.1}s", item.elapsed_ms as f64 / 1_000.0);
    }
    output.push('\n');
    if let Some(reason) = &item.reason {
        let _ = writeln!(output, "             {reason}");
    }
    if item.answer == "broken" {
        for line in item.output.lines() {
            let _ = writeln!(output, "             | {line}");
        }
    }
    output
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
        mission.state != MissionState::Retired,
        "mission `mission/{mission_id}` is retired; publish a ready revision to start it again"
    );
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
            print!("{}", cli_help::mission_next_steps(&started));
            Ok(())
        };
    }
    if !json_output {
        print!("{}", cli_help::mission_next_steps(&started));
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

/// Free mode ignores authority blocks; say so on stderr when a publication still carries them.
fn warn_ignored_authority(preview: &MissionResponse) {
    for warning in &preview.warnings {
        if let Some(warning) = warning.strip_prefix("free-mode: ") {
            eprintln!("st: {warning}");
        }
    }
}

async fn publish_text(
    client: &Client,
    kdl: String,
    source_name: String,
    actor: String,
) -> Result<ApplyResponse> {
    publish_text_with_expected(client, kdl, source_name, actor, None).await
}

async fn publish_text_with_expected(
    client: &Client,
    kdl: String,
    source_name: String,
    actor: String,
    expected: Option<(&str, &[String])>,
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
    warn_ignored_authority(&mission);
    if let Some((subject, tokens)) = expected {
        anyhow::ensure!(
            mission.subject_tokens.get(subject).map(Vec::as_slice) == Some(tokens),
            "the declaration for `{subject}` changed while preparing start; retry the command"
        );
    }
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
    pty_root: Option<&Path>,
    command: PtyCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        PtyCommand::New(args) => {
            let person =
                configured_actor(args.person.as_deref(), configured_person, "terminals new")?;
            let generated = generated_client(endpoint, Some(&person))?;
            let capabilities = generated.capabilities().await?;
            let local = client.get::<Value>("/v1/health").await?["node"]
                .as_str()
                .context("daemon has no node")?
                .to_owned();
            let cwd = if let Some(cwd) = args.cwd {
                if args
                    .host
                    .as_deref()
                    .is_none_or(|host| host.trim_start_matches("host/") == local)
                {
                    Some(std::path::absolute(cwd)?.display().to_string())
                } else {
                    anyhow::ensure!(
                        cwd.is_absolute(),
                        "remote cwd must be an absolute path on its host"
                    );
                    Some(cwd.display().to_string())
                }
            } else if args
                .host
                .as_deref()
                .is_none_or(|host| host.trim_start_matches("host/") == local)
            {
                Some(std::env::current_dir()?.display().to_string())
            } else {
                None
            };
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let result = generated
                .terminal_create(
                    format!("action/{nonce}"),
                    format!("terminal-new:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        ..ClientFence::default()
                    },
                    st3_client::TerminalCreateParameters {
                        name: args
                            .name
                            .unwrap_or_else(|| format!("shell-{}", &nonce[24..])),
                        host: args.host,
                        cwd,
                    },
                )
                .await?;
            if json_output {
                print_client_value(&result, true)
            } else {
                println!(
                    "{}",
                    result
                        .value
                        .affected_ids
                        .first()
                        .context("creation returned no terminal")?
                );
                Ok(())
            }
        }
        PtyCommand::End(args) => {
            let person =
                configured_actor(args.person.as_deref(), configured_person, "terminals end")?;
            let generated = generated_client(endpoint, Some(&person))?;
            let capabilities = generated.capabilities().await?;
            let nonce = uuid::Uuid::now_v7().simple().to_string();
            let result = generated
                .terminal_end(
                    format!("action/{nonce}"),
                    format!("terminal-end:{nonce}"),
                    ClientFence {
                        snapshot_id: capabilities.snapshot.id,
                        ..ClientFence::default()
                    },
                    ClientTargetParameters {
                        target_id: args.subject,
                        ..ClientTargetParameters::default()
                    },
                )
                .await?;
            print_client_value(&result, json_output)
        }
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
            let person = args.person.as_deref().or(configured_person);
            attach_terminal(client, endpoint, pty_root, person, &subject, args.force).await
        }
        PtyCommand::ExposeFabric(_) | PtyCommand::ServeFabric(_) => {
            unreachable!("handled before any daemon client")
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
            let person = configured_actor(
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
            let person = configured_actor(
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
            let person = configured_actor(
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
            let person = configured_actor(
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
            let person = configured_actor(
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

/// How long `st terminals attach` waits for its daemon to choose among a terminal's PTY sessions
/// on this host before it attaches to the newest one without the daemon.
const LOCAL_ATTACH_CONSULT: Duration = Duration::from_secs(1);

/// The daemon's answer to attaching one terminal.
enum AttachAnswer {
    /// A PTY session on this host that the daemon selected and this CLI connects to itself.
    Local(st3::model::LocalTerminal),
    /// The daemon's WebSocket bridge: an HTTP endpoint, or a daemon from before direct
    /// attachment.
    Bridge(Attachment),
}

/// Ask the daemon behind `client` how to attach `subject`.
async fn consult_attach(client: &Client, subject: &str) -> Result<AttachAnswer> {
    if let Some(terminal) = client.local_terminal(subject).await? {
        return Ok(AttachAnswer::Local(terminal));
    }
    client
        .post(
            &format!("/v1/sessions/attach/{}", urlencoding::encode(subject)),
            &AttachRequest::default(),
        )
        .await
        .map(AttachAnswer::Bridge)
}

/// Attach this terminal to one terminal member. A terminal on this host is attached through its
/// PTY session; one that another fleet host owns goes through the client gateway, the path paired
/// clients use, as `person`. `pty_root` is this host's PTY root, whichever endpoint answers.
async fn attach_terminal(
    client: &Client,
    endpoint: &Endpoint,
    pty_root: Option<&Path>,
    person: Option<&str>,
    subject: &str,
    force: bool,
) -> Result<()> {
    if !force
        && let Ok(outer) = std::env::var("PTY_SESSION")
        && !outer.is_empty()
    {
        anyhow::bail!(
            "st terminals attach: already inside PTY session `{outer}`. Detach first with Ctrl+\\, or pass --force."
        );
    }
    // The subject's running PTY sessions on this host, read from the registry alone. When there
    // are any, the daemon has a moment to choose one and refuse; a daemon that cannot answer that
    // fast must not keep anyone from debugging this host.
    let local = pty_root
        .map(|root| (root, st3::client::tagged_pty_sessions(root, subject)))
        .filter(|(_, sessions)| !sessions.is_empty());
    let daemon = format!("st daemon at {}", endpoint_label(endpoint));
    let mut consult = std::pin::pin!(consult_attach(client, subject));
    let answer = match tokio::time::timeout(LOCAL_ATTACH_CONSULT, consult.as_mut()).await {
        Ok(answer) => answer,
        Err(_) => match (&local, pty_root) {
            (Some((root, sessions)), _) => {
                let waited = format!(
                    "the {daemon} did not answer within {} ms",
                    LOCAL_ATTACH_CONSULT.as_millis()
                );
                let code = attach_unconsulted(root, subject, sessions, &waited).await?;
                return terminal_exit(code);
            }
            (None, Some(root)) => {
                eprintln!(
                    "st terminals attach: waiting for the {daemon}; no PTY session of `{subject}` runs under {} to attach without it.",
                    root.display()
                );
                consult.await
            }
            (None, None) => consult.await,
        },
    };
    let code = match answer {
        Ok(AttachAnswer::Local(terminal)) => st3::client::attach_local_terminal(&terminal).await?,
        Ok(AttachAnswer::Bridge(attachment)) => {
            client
                .proxy_terminal_resilient(subject, &attachment)
                .await?
        }
        Err(error) if st3::client::api_error_code(&error) == Some("runtime-not-local") => {
            attach_remote_terminal(client, endpoint, person, subject, error).await?
        }
        Err(error) => match &local {
            Some((root, sessions)) if st3::client::daemon_did_not_answer(&error) => {
                let failed = format!("the {daemon} did not answer ({error:#})");
                attach_unconsulted(root, subject, sessions, &failed).await?
            }
            _ => return Err(error),
        },
    };
    terminal_exit(code)
}

fn endpoint_label(endpoint: &Endpoint) -> String {
    match endpoint {
        Endpoint::Unix(socket) => socket.display().to_string(),
        Endpoint::Http(url) => url.clone(),
    }
}

fn terminal_exit(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(CommandExit(code.clamp(1, 255) as u8).into())
    }
}

/// Attach to a terminal that another fleet host owns, as `person`: PTY to PTY over Fabric when
/// Fabric reaches the owner, and otherwise through the client gateway. `not_local` is the local
/// daemon's refusal, which names the owner.
async fn attach_remote_terminal(
    client: &Client,
    endpoint: &Endpoint,
    person: Option<&str>,
    subject: &str,
    not_local: anyhow::Error,
) -> Result<i32> {
    let person = person.with_context(|| {
        format!(
            "{not_local:#}. Attaching to it from this host needs `--as person/NAME` or `person = \"person/NAME\"` in the st config"
        )
    })?;
    let person = parse_actor_subject(person).map_err(anyhow::Error::msg)?;
    let gateway = generated_client(endpoint, Some(&person))?;
    let unreached = match fabric_route(client, &gateway, subject).await {
        Ok((target, request)) => match st3::terminal_fabric::attach(&target, &request).await? {
            Ok(code) => return Ok(code),
            Err(error) => format!("Fabric did not reach its PTY session: {error}"),
        },
        Err(error) => format!("{error:#}"),
    };
    eprintln!("st terminals attach: {unreached}. Attaching through the client gateway instead.");
    st3::remote_terminal::attach(&gateway, subject, subject).await
}

/// The Fabric route to the PTY session of `subject`, a terminal another fleet host owns. st
/// first checks that the gateway grants its person terminal reading and control, then reads the
/// runtime and incarnation this daemon holds for it. The owner proves that incarnation before
/// it passes a byte.
async fn fabric_route(
    client: &Client,
    gateway: &GeneratedClient,
    subject: &str,
) -> Result<(
    st3::terminal_fabric::FabricTarget,
    st3::terminal_fabric::RouteRequest,
)> {
    let capabilities = gateway.capabilities().await?.value;
    for scope in ["terminal.read", "terminal.control"] {
        anyhow::ensure!(
            capabilities.capabilities.iter().any(|capability| {
                capability.id == scope && capability.state == st3_client::CapabilityState::Granted
            }),
            "the client gateway does not grant `{scope}` for a direct attachment"
        );
    }
    let status = status_for(client, subject).await?;
    let selected = status
        .subjects
        .first()
        .with_context(|| format!("st has no runtime for `{subject}`"))?;
    let owner = selected
        .actual_origin
        .as_deref()
        .with_context(|| format!("st does not know which host runs `{subject}`"))?;
    let fields = selected
        .actual
        .as_ref()
        .map(|actual| actual.get("fields").unwrap_or(actual))
        .with_context(|| format!("st has no runtime for `{subject}`"))?;
    let field = |name: &str| fields.get(name).and_then(Value::as_str);
    anyhow::ensure!(
        field("status") == Some("running") && fields.get("terminal") != Some(&Value::Bool(false)),
        "`{subject}` has no running terminal on `{owner}`"
    );
    let (Some(runtime_id), Some(incarnation)) = (field("runtime_id"), field("incarnation_id"))
    else {
        anyhow::bail!("st has no runtime incarnation for `{subject}` on `{owner}`");
    };
    let (fabric, fleet_id) = configured_fabric()?;
    let fleet_id = fleet_id
        .with_context(|| format!("this machine is in no fleet to reach `{owner}` through"))?;
    let fabric =
        fabric.with_context(|| format!("this machine has no `fabric` to reach `{owner}` with"))?;
    let view: st3::fleet::FleetView = client.get("/v1/internal/fleet/membership").await?;
    let peer = st3::terminal_fabric::owner_peer(&fabric, &view.members, owner)
        .await
        .with_context(|| {
            format!("`{owner}` advertises no Fabric node, and no Fabric peer of this machine has its name")
        })?;
    Ok((
        st3::terminal_fabric::FabricTarget {
            fabric,
            peer,
            protocol: st3::terminal_fabric::protocol(&fleet_id),
        },
        st3::terminal_fabric::RouteRequest::new(runtime_id, subject, incarnation),
    ))
}

/// This machine's `fabric`, the fleet file's override or the one on `PATH`, and its fleet.
fn configured_fabric() -> Result<(Option<st3::fleet::transport::Fabric>, Option<String>)> {
    let mut config = Config::load_unvalidated(None)?;
    config.apply_fleet_file()?;
    let override_path = config
        .fleet
        .as_ref()
        .and_then(|file| file.fabric.as_deref());
    let fabric = st3::fleet::transport::resolve_tool(override_path, "fabric")
        .map(st3::fleet::transport::Fabric::new);
    Ok((fabric, config.fleet_id))
}

/// `st terminals expose-fabric`: have this machine's Fabric serve the fleet's PTY sessions.
async fn expose_fabric(config: &Config, args: PtyExposeFabricArgs) -> Result<()> {
    let (fabric, fleet_id) = configured_fabric()?;
    let fleet_id =
        fleet_id.context("this machine is in no fleet, so no peer can attach its terminals")?;
    let fabric = fabric.context("`fabric` is not on PATH")?;
    let st = match args.st {
        Some(st) => std::path::absolute(st)?,
        None => std::env::current_exe().context("find this st executable")?,
    };
    let pty_root = std::path::absolute(
        config
            .pty_root
            .clone()
            .unwrap_or_else(|| config.state_dir.join("pty")),
    )?;
    let protocol = st3::terminal_fabric::protocol(&fleet_id);
    let (st, pty_root) = (st.display().to_string(), pty_root.display().to_string());
    let argv = [
        st.as_str(),
        "terminals",
        "serve-fabric",
        "--stdio",
        "--pty-root",
        pty_root.as_str(),
    ];
    fabric.expose_exec(&protocol, &argv).await?;
    println!("Fabric serves the PTY sessions under {pty_root} as `{protocol}`, with `{st}`.");
    println!(
        "Add `{protocol}` to the `allow` list of each peer in this machine's Fabric peers.toml, then run `fabric reload-peers`."
    );
    Ok(())
}

/// Attach to the newest of `subject`'s running PTY sessions on this host without the st daemon,
/// as the local user who owns the PTY root. The note says st was not consulted, and names any
/// other session a person may have meant.
async fn attach_unconsulted(
    pty_root: &Path,
    subject: &str,
    sessions: &[st3::client::TaggedPtySession],
    why: &str,
) -> Result<i32> {
    let (newest, others) = sessions
        .split_first()
        .context("an attachment without st needs a PTY session")?;
    eprintln!(
        "st terminals attach: {why}, so st was not consulted. Attaching as the local user to PTY session `{}` of `{subject}` (started {}) under {}, without st's incarnation check.",
        newest.runtime_id,
        newest.created_at,
        pty_root.display()
    );
    for other in others {
        eprintln!(
            "  also running: `{}` (started {})",
            other.runtime_id, other.created_at
        );
    }
    st3::client::attach_unconsulted_terminal(pty_root, subject, newest).await
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
        print_value(&response, true)?;
    } else {
        print!("{}", render_now_page(&response.value, &command));
    }
    note_partial_page(&response.value);
    Ok(())
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
                    person_id: person.clone(),
                    full_control: full_control.then_some(true),
                })
                .await?;
            print_client_value(&response, json_output)?;
            if !json_output {
                print!("{}", cli_help::pairing_next_steps(&person));
            }
            Ok(())
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

/// The actor of a terminal or client-v0 command: an explicit person or agent, else the harness's
/// own seat, else the configured person. Free mode lets an agent act, always as itself.
fn configured_actor(
    explicit: Option<&str>,
    configured: Option<&str>,
    command: &str,
) -> Result<String> {
    if let Some(explicit) = explicit {
        return parse_actor_subject(explicit).map_err(anyhow::Error::msg);
    }
    if let Some(own) = std::env::var("ST_AGENT")
        .ok()
        .filter(|own| own.starts_with("agent/"))
    {
        return Ok(own);
    }
    configured_human(None, configured, command)
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
        print_value(response, true)?;
    } else {
        if let Some(sync) = &response.value.sync {
            print!("{}", render_sync_notice(sync, now_ms()));
        }
        print!(
            "{}",
            render_product_page(title, &response.value, continuation_command)
        );
    }
    note_partial_page(&response.value);
    Ok(())
}

/// Tell a gate check that this command printed only the first part of `page`'s collection. It
/// runs after the page is printed: a reader that stopped early, such as `grep -q` on a match,
/// ends the command before it reports a listing whose rest did not matter.
fn note_partial_page(page: &ClientPage) {
    if page.page.has_more {
        st3::gate_report::note_partial_listing(page.items.len());
    }
}

async fn run_collection_watch(
    endpoint: &Endpoint,
    person: Option<&str>,
    collection: &str,
    actor: Option<&str>,
    status: Option<&str>,
    limit: usize,
    title: &str,
    json_output: bool,
) -> Result<()> {
    let client = generated_client(endpoint, person)?;
    let mut rows = BTreeMap::<String, Value>::new();
    loop {
        let mut stream = match client.collection_stream().await {
            Ok(stream) => stream,
            Err(GeneratedClientError::Transport(_) | GeneratedClientError::Unreachable(_)) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        stream
            .subscribe("list", collection, limit, actor, status)
            .await?;
        loop {
            let frame = match stream.next().await {
                Ok(Some(frame)) => frame,
                Ok(None)
                | Err(GeneratedClientError::Transport(_))
                | Err(GeneratedClientError::Unreachable(_)) => break,
                Err(error) => return Err(error.into()),
            };
            match frame["kind"].as_str() {
                Some("error") => anyhow::bail!(
                    "collection watch: {}",
                    frame["message"].as_str().unwrap_or("unknown error")
                ),
                Some("resync") => break,
                Some("snapshot") => {
                    rows.clear();
                    for item in frame["items"].as_array().into_iter().flatten() {
                        if let Some(id) = item["id"].as_str() {
                            rows.insert(id.to_owned(), item.clone());
                        }
                    }
                }
                Some("changes") => {
                    for id in frame["removes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        rows.remove(id);
                    }
                    for item in frame["upserts"].as_array().into_iter().flatten() {
                        if let Some(id) = item["id"].as_str() {
                            rows.insert(id.to_owned(), item.clone());
                        }
                    }
                }
                _ => continue,
            }
            let order = frame["order"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if json_output {
                print_value(&frame, true)?;
                continue;
            }
            let items = order
                .iter()
                .filter_map(|id| rows.get(id).cloned())
                .collect::<Vec<_>>();
            let page: ClientEnvelope<ClientPage> = serde_json::from_value(json!({
                "api_version": CLIENT_V0_API_VERSION,
                "request_id": "watch",
                "snapshot": frame["snapshot"],
                "value": {
                    "kind": "page", "collection": collection, "filters": {}, "items": items,
                    "page": {"limit":limit, "has_more":frame["has_more"], "next_cursor":null},
                    "sync": null
                }
            }))?;
            if std::io::stdout().is_terminal() {
                print!("\x1b[2J\x1b[H");
            }
            if collection == "agents" {
                print!(
                    "{}",
                    render_client_agents(&page.value, false, false, "st agents ls --watch")
                );
            } else {
                print_product_page(title, &page, false, &format!("st {collection} ls --watch"))?;
            }
            std::io::stdout().flush()?;
        }
        // The socket ended (including daemon restart). Reopen and subscribe for
        // a fresh snapshot rather than attempting to resume a stale projection.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A host catching up with a peer shows early history as current, and a host whose graph
/// diverged from a peer's can show it wrong, so say so before the items.
fn render_sync_notice(sync: &st3_client::SyncNotice, now: u128) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    for peer in &sync.peers {
        if let Some(since) = &peer.diverged_since {
            let since = chrono::DateTime::parse_from_rfc3339(since)
                .map(|at| relative_time(at.timestamp_millis().max(0) as u128, now))
                .unwrap_or_else(|_| since.clone());
            let _ = writeln!(output, "DIVERGED  {} · since {since}", peer.summary());
            continue;
        }
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
        "{}",
        if sync.diverged() {
            "  Exchanges cannot fix this, so items below can be wrong. Details: st3 replication status\n"
        } else {
            "  Until then, items below can be out of date. Progress: st3 replication status\n"
        }
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
    let total = mission.total_runs.unwrap_or(mission.runs.len());
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
                if item.runs_truncated {
                    let _ = writeln!(output, "  inspect: st missions show {}", item.header.id);
                } else if let Some(run) = item.runs.last() {
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

async fn run_usage(client: &Client, args: UsageArgs, json_output: bool) -> Result<()> {
    anyhow::ensure!(args.hours > 0, "usage hours must be positive");
    let until = current_unix_ms()? as u64;
    let since = until.saturating_sub(args.hours.saturating_mul(3_600_000));
    let report: Value = client
        .get(&format!("/v1/usage?since_ms={since}&until_ms={until}"))
        .await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print!("{}", render_usage_report(&report, args.hours, args.by));
    Ok(())
}

/// A limits row's account: the declared name beside the provider's label, or the label alone.
fn account_name(label: &Value, declared: &Value) -> String {
    let label = label.as_str().unwrap_or("unknown");
    match declared.as_str() {
        Some(name) => format!("{name} ({label})"),
        None => label.to_owned(),
    }
}

fn render_usage_report(report: &Value, hours: u64, only: Option<UsageBy>) -> String {
    use std::fmt::Write as _;

    const COLUMNS: [&str; 7] = [
        "cost_microusd",
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "cache_write_tokens",
        "cached_tokens",
        "unpriced_tokens",
    ];
    let rows = report["rows"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let sum = |rows: &mut dyn Iterator<Item = &Value>| {
        let mut values = [0_u64; COLUMNS.len()];
        for row in rows {
            for (value, field) in values.iter_mut().zip(COLUMNS) {
                *value = value.saturating_add(row[field].as_u64().unwrap_or(0));
            }
        }
        values
    };
    // Cost is API-equivalent: what the tokens would cost at list price. A trailing `+` marks a
    // figure that leaves out tokens on models without a price.
    let dollars = |values: &[u64; COLUMNS.len()]| {
        format!(
            "${:.2}{}",
            values[0] as f64 / 1_000_000.0,
            if values[6] > 0 { "+" } else { "" }
        )
    };
    let mut output = String::new();
    let total = sum(&mut rows.iter());
    let _ = writeln!(
        output,
        "SPEND  {} · {} tokens · {hours}h · API-equivalent",
        dollars(&total),
        total[1]
    );
    if total[6] > 0 {
        let _ = writeln!(
            output,
            "UNPRICED  {} tokens on models without a price",
            total[6]
        );
    }
    let limits = report["limits"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !limits.is_empty() && only.is_none() {
        let percent = |value: &Value| {
            value
                .as_f64()
                .map_or_else(|| "?".to_owned(), |value| format!("{value:.0}%"))
        };
        let time = |value: &Value| {
            value
                .as_u64()
                .and_then(|at| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at as i64))
                .map_or_else(
                    || "?".to_owned(),
                    |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
                )
        };
        output.push('\n');
        let _ = writeln!(
            output,
            "LIMITS  {} · the freshest reading of each account",
            limits.len()
        );
        let _ = writeln!(output, "WEEKLY  5-HOUR  WEEKLY RESET  MEASURED  account");
        for limit in limits {
            let _ = writeln!(
                output,
                "{}  {}  {}  {}  {}",
                percent(&limit["weekly_percent"]),
                percent(&limit["five_hour_percent"]),
                time(&limit["weekly_resets_at_unix_ms"]),
                time(&limit["measured_at_unix_ms"]),
                account_name(&limit["account"], &limit["account_ref"]),
            );
        }
    }
    // A harness bound to a declared account reports that account's name beside the provider's
    // label, so usage reads against the accounts a person declared.
    let declared = limits
        .iter()
        .filter_map(|limit| Some((limit["account"].as_str()?, limit["account_ref"].as_str()?)))
        .collect::<BTreeMap<_, _>>();
    let groups = only
        .map(|by| vec![by])
        .unwrap_or_else(|| UsageBy::ALL.to_vec());
    for by in groups {
        let mut totals = BTreeMap::<String, Vec<&Value>>::new();
        for row in rows {
            let label = row[by.field()]
                .as_str()
                .filter(|value| !value.is_empty())
                .unwrap_or("unknown");
            let label = match (by, declared.get(label)) {
                (UsageBy::Account, Some(name)) => format!("{name} ({label})"),
                _ => label.to_owned(),
            };
            totals.entry(label).or_default().push(row);
        }
        let mut totals = totals
            .into_iter()
            .map(|(name, rows)| (name, sum(&mut rows.into_iter())))
            .collect::<Vec<_>>();
        totals.sort_by(|left, right| {
            right.1[0]
                .cmp(&left.1[0])
                .then_with(|| right.1[1].cmp(&left.1[1]))
                .then_with(|| left.0.cmp(&right.0))
        });
        let by = by.label();
        output.push('\n');
        let _ = writeln!(output, "USAGE  {} · {}h · by {by}", totals.len(), hours);
        let _ = writeln!(
            output,
            "COST  TOTAL  INPUT  OUTPUT  CACHE WRITE  CACHE READ  {by}"
        );
        for (name, values) in totals {
            let _ = writeln!(
                output,
                "{}  {}  {}  {}  {}  {}  {name}",
                dollars(&values),
                values[1],
                values[2],
                values[3],
                values[4],
                values[5]
            );
        }
    }
    output
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
    use std::io::IsTerminal as _;
    let stdout = std::io::stdout();
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

fn conversation_text(
    session_id: &str,
    items: &[ClientTimelineEntry],
    width: usize,
    color: bool,
    density: st3_conversation_ui::Density,
) -> String {
    let mut output = format!("CONVERSATION  {session_id}\n\n");
    if let Some(reason) = st3_conversation_ui::adapt::unreadable_transcript(items) {
        output.push_str(&format!("{reason}\n"));
    }
    let entries = st3_conversation_ui::adapt::conversation(items, &Default::default());
    if entries.is_empty() {
        output.push_str("Nothing in this conversation yet.\n");
        return output;
    }
    let rendered = st3_conversation_ui::Cache::default().render_as(
        &entries,
        width,
        &Default::default(),
        "",
        &st3_conversation_ui::Theme::default(),
        density,
    );
    for line in &rendered.lines {
        output.push_str(&st3_conversation_ui::ansi::line(line, color));
        output.push('\n');
    }
    output
}

/// The terminal's width in columns, when stdout is one.
fn terminal_columns() -> Option<usize> {
    if let Some(columns) = std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return Some(columns);
    }
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    (ok && size.ws_col > 0).then_some(size.ws_col as usize)
}

fn timeline_entries_text(session_id: &str, items: &[ClientTimelineEntry]) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    let _ = writeln!(
        output,
        "CONVERSATION  {} · {} entries",
        session_id,
        items.len()
    );
    if items.is_empty() {
        let _ = writeln!(output, "No normalized timeline entries.");
    }
    for entry in items {
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
                if let (Some(from), Some(to)) = (&body.from, &body.to) {
                    let _ = write!(output, " · {from} → {to}");
                }
                if let Some(title) = &body.title {
                    let _ = write!(output, " · {title}");
                }
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
    output
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
    // Small Talk and transcript entries number their sequences apart; time orders them.
    changed.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then(a.sequence.cmp(&b.sequence))
    });
    changed
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

async fn run_subject(client: &Client, command: SubjectCommand, json_output: bool) -> Result<()> {
    match command {
        SubjectCommand::Show(args) => {
            if args.kdl {
                anyhow::ensure!(
                    args.subject.starts_with("agent/"),
                    "--kdl is supported for managed agents only"
                );
                let status = status_for(client, &args.subject).await?;
                let mut desired = status
                    .subjects
                    .iter()
                    .find(|subject| subject.subject == args.subject)
                    .and_then(|subject| subject.desired.as_ref())
                    .cloned()
                    .context("No managed declaration")?;
                if !args.show_env_values {
                    st3::graph::redact_agent_env_values(&mut desired);
                }
                print!("{}", st3::graph::render_agent_desired_kdl(&desired)?);
                Ok(())
            } else {
                run_inspect(
                    client,
                    InspectArgs {
                        subject: args.subject,
                    },
                    json_output,
                )
                .await
            }
        }
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
    // Capture the current index before checking the condition. A change made during the
    // check is then still visible to the event wait, without reading all prior events.
    let health: Value = client.get("/v1/health").await?;
    let mut cursor = health["store_index"]
        .as_u64()
        .context("the daemon health response has no store index")?;
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

fn outcome_time(value: &str, now: u128) -> Result<u128> {
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(value) {
        return u128::try_from(at.timestamp_millis())
            .context("the time must be after the Unix epoch");
    }
    let ago = parse_timeout(value).context("use a duration such as 6h or an RFC3339 timestamp")?;
    Ok(now.saturating_sub(ago.as_millis()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct OutcomeCursor {
    collection: String,
    since: u128,
    until: u128,
    status: Option<String>,
    actor: Option<String>,
    before: u64,
    limit: usize,
}

async fn list_outcomes(
    client: &Client,
    collection: &str,
    since: Option<String>,
    until: Option<String>,
    status: Option<String>,
    actor: Option<String>,
    cursor: Option<String>,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let now = current_unix_ms()?;
    let mut filter = if let Some(cursor) = cursor {
        let encoded = cursor
            .strip_prefix("outcomes:")
            .context("use the outcome cursor printed by this command")?;
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)?;
        let saved: OutcomeCursor = serde_json::from_slice(&decoded)?;
        anyhow::ensure!(
            saved.collection == collection
                && saved.limit == limit
                && saved.actor == actor
                && status
                    .as_deref()
                    .is_none_or(|s| saved.status.as_deref() == Some(s)),
            "the outcome cursor does not match the collection or filters"
        );
        anyhow::ensure!(
            since.is_none() && until.is_none(),
            "the cursor already fixes the time window; omit --since and --until"
        );
        saved
    } else {
        OutcomeCursor {
            collection: collection.into(),
            since: since
                .as_deref()
                .map(|v| outcome_time(v, now))
                .transpose()?
                .unwrap_or(0),
            until: until
                .as_deref()
                .map(|v| outcome_time(v, now))
                .transpose()?
                .unwrap_or(now),
            status,
            actor,
            before: i64::MAX as u64,
            limit,
        }
    };
    anyhow::ensure!(
        filter.since <= filter.until,
        "--since must be before --until"
    );
    let mut path = format!(
        "/v1/outcome-history?collection={collection}&since={}&until={}&before={}&limit={limit}",
        filter.since, filter.until, filter.before
    );
    if let Some(status) = &filter.status {
        path.push_str(&format!("&status={}", urlencoding::encode(status)));
    }
    if let Some(actor) = &filter.actor {
        path.push_str(&format!("&actor={}", urlencoding::encode(actor)));
    }
    let mut page: Value = client.get(&path).await?;
    if let Some(before) = page["next_before"].as_u64() {
        filter.before = before;
        let next = format!(
            "outcomes:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&filter)?)
        );
        page["next_cursor"] = json!(next);
    }
    let partial = page["has_more"].as_bool().unwrap_or(false);
    let shown = page["items"].as_array().map_or(0, Vec::len);
    if json_output {
        print_value(&page, true)?;
        if partial {
            st3::gate_report::note_partial_listing(shown);
        }
        return Ok(());
    }
    let items = page["items"].as_array().context("invalid outcome page")?;
    println!("{} OUTCOMES  {}", collection.to_uppercase(), items.len());
    for item in items {
        println!(
            "{}  {}  {}",
            item["subject"].as_str().unwrap_or("?"),
            item["status"].as_str().unwrap_or("?"),
            relative_time(item["at_unix_ms"].as_u64().unwrap_or(0) as u128, now)
        );
        if let Some(reason) = item["reason"].as_str() {
            println!("  reason: {reason}");
        }
        if let Some(mission) = item["mission"].as_str() {
            println!("  mission: {mission}");
        }
    }
    if let Some(cursor) = page["next_cursor"].as_str() {
        let actor = filter
            .actor
            .as_deref()
            .map(|a| format!(" --as {a}"))
            .unwrap_or_default();
        println!("Next: st {collection} ls{actor} --limit {limit} --cursor '{cursor}'");
    }
    if partial {
        st3::gate_report::note_partial_listing(shown);
    }
    Ok(())
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

fn render_performance(view: &Value) -> String {
    use std::fmt::Write as _;
    if view.is_null() {
        return String::new();
    }
    let mut out = format!(
        "PERFORMANCE  last {} seconds · sorted by total wall time\n",
        view["window_seconds"]
    );
    for (key, title) in [("requests", "REQUESTS AND TASKS"), ("queries", "QUERIES")] {
        let _ = writeln!(out, "{title}  count · total ms · max ms · CPU ms · kind");
        if let Some(rows) = view[key].as_array() {
            for row in rows {
                let _ = writeln!(
                    out,
                    "  {}  {:.1}  {:.1}  {:.1}  {}",
                    row["count"],
                    row["total_ms"].as_f64().unwrap_or(0.0),
                    row["max_ms"].as_f64().unwrap_or(0.0),
                    row["cpu_ms"].as_f64().unwrap_or(0.0),
                    row["kind"].as_str().unwrap_or("?")
                );
            }
        }
    }
    if let Some(count) = view["request_count"].as_u64() {
        let seconds = view["sampled_seconds"].as_u64().unwrap_or(300).max(1);
        let _ = writeln!(
            out,
            "REQUESTS BY CLIENT  {count} requests in {seconds} s ({:.1}/s) · sorted by count",
            count as f64 / seconds as f64
        );
    }
    for (key, title) in [
        ("clients", "CLIENTS  count · total ms · CPU ms · client"),
        (
            "client_requests",
            "CLIENT REQUESTS  count · total ms · CPU ms · client · kind",
        ),
    ] {
        let Some(rows) = view[key].as_array() else {
            continue;
        };
        let _ = writeln!(out, "{title}");
        for row in rows {
            let _ = write!(
                out,
                "  {}  {:.1}  {:.1}  {}",
                row["count"],
                row["total_ms"].as_f64().unwrap_or(0.0),
                row["cpu_ms"].as_f64().unwrap_or(0.0),
                row["client"].as_str().unwrap_or("?")
            );
            if let Some(kind) = row["kind"].as_str() {
                let _ = write!(out, "  {kind}");
            }
            out.push('\n');
        }
    }
    if let Some(corrections) = view["incremental_corrections"]
        .as_array()
        .filter(|rows| !rows.is_empty())
    {
        let _ = writeln!(
            out,
            "INCREMENTAL CORRECTIONS  count · item (writes an incremental pass would have missed; each is a bug)"
        );
        for row in corrections {
            let _ = writeln!(
                out,
                "  {}  {}",
                row["count"],
                row["item"].as_str().unwrap_or("?")
            );
        }
    }
    if let Some(evaluations) = view["incremental_evaluations"]
        .as_array()
        .filter(|rows| !rows.is_empty())
    {
        let _ = writeln!(
            out,
            "RECONCILE EVALUATIONS  count · CPU ms · item · whether an incremental pass would run it"
        );
        for row in evaluations {
            let _ = writeln!(
                out,
                "  {}  {:.1}  {}",
                row["count"],
                row["cpu_ms"].as_f64().unwrap_or(0.0),
                row["item"].as_str().unwrap_or("?")
            );
        }
    }
    if let Some(wakes) = view["reconciler_wakes"].as_array() {
        let _ = writeln!(
            out,
            "RECONCILER WAKES  count · cause (a pass can serve several wakes)"
        );
        for row in wakes {
            let _ = writeln!(
                out,
                "  {}  {}",
                row["count"],
                row["cause"].as_str().unwrap_or("?")
            );
        }
    }
    if let Some(note) = view["query_time_note"].as_str() {
        let _ = writeln!(out, "{note}");
    }
    out
}

async fn run_doctor(client: &Client, args: DoctorArgs, json_output: bool) -> Result<()> {
    if args.performance {
        let report: Value = client.get("/v1/performance").await?;
        if json_output {
            return print_value(&report, true);
        }
        print!("{}", render_performance(&report));
        return Ok(());
    }
    let report: DoctorReport = client.get("/v1/doctor").await?;
    if json_output {
        print_value(&report, true)?;
    } else {
        for check in &report.checks {
            println!("{}\t{}\t{}", check.status, check.name, check.message);
        }
        print!("{}", render_performance(&report.performance));
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

/// Wait for this node's first sync to end, printing progress when `progress` is set. It ends at
/// the first exchange at which this node holds the same envelopes as a peer. With matching
/// registries the graphs must also match, at once or after a heal. Returns None on timeout.
async fn wait_for_first_sync(
    client: &Client,
    timeout: Duration,
    progress: bool,
) -> Result<Option<st3::model::ReplicationFirstSync>> {
    let started = std::time::Instant::now();
    let mut reported = None::<std::time::Instant>;
    loop {
        if let Ok(status) = client
            .get::<ReplicationStatus>("/v1/replication/status")
            .await
        {
            // A removed node never finishes syncing; say why instead of waiting out the timeout.
            if let Some(removed) = &status.removed {
                anyhow::bail!("{}", removed.describe(status.fleet_id.as_deref()));
            }
            let first = status.first_sync.clone().context(
                "this node has no first sync to wait for: it did not join with st fleet join",
            )?;
            match first.state.as_str() {
                "verified" | "failed" => return Ok(Some(first)),
                _ => {
                    if progress && reported.is_none_or(|at| at.elapsed() >= Duration::from_secs(10))
                    {
                        reported = Some(std::time::Instant::now());
                        let behind = status
                            .peers
                            .iter()
                            .filter_map(|peer| {
                                let sync = peer.sync.as_ref()?;
                                Some(format!(
                                    "{} has {} this node lacks",
                                    peer.peer,
                                    envelope_count(sync.peer_only_envelopes)
                                ))
                            })
                            .collect::<Vec<_>>();
                        println!(
                            "first sync: {} envelopes so far{}",
                            status.received_envelopes,
                            if behind.is_empty() {
                                String::new()
                            } else {
                                format!("; {}", behind.join("; "))
                            }
                        );
                    }
                }
            }
        }
        if started.elapsed() >= timeout {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The peers this node caught up with at exchanges since a wait began, and those it could not
/// check because they are not up.
#[derive(Debug, Default, PartialEq, serde::Serialize)]
struct CaughtUp {
    /// Each peer and when this node last measured that it held every envelope the peer held.
    peers: Vec<(String, u128)>,
    not_checked: Vec<String>,
}

/// Whether this node has caught up since `since`: every peer that is up has exchanged since
/// then, and at the latest exchange this node held every envelope that peer held. Peers that are
/// not up cannot be checked, but at least one peer must be. `Err` says what is still missing.
fn caught_up_since(status: &ReplicationStatus, since: u128) -> Result<CaughtUp, String> {
    let mut caught_up = CaughtUp::default();
    let mut waiting = Vec::new();
    for peer in &status.peers {
        let fresh = peer
            .sync
            .as_ref()
            .filter(|sync| sync.measured_at_unix_ms >= since);
        match fresh {
            Some(sync) if sync.peer_only_envelopes == 0 => caught_up
                .peers
                .push((peer.peer.clone(), sync.measured_at_unix_ms)),
            Some(sync) => waiting.push(format!(
                "{} has {} this node lacks",
                peer.peer,
                envelope_count(sync.peer_only_envelopes)
            )),
            None if peer.status == "up" => {
                waiting.push(format!("no exchange with {} yet", peer.peer));
            }
            None => caught_up
                .not_checked
                .push(format!("{} ({})", peer.peer, peer.status)),
        }
    }
    if caught_up.peers.is_empty() && waiting.is_empty() {
        waiting.push(if status.peers.is_empty() {
            "this node has no peers".into()
        } else {
            "no peer has exchanged with this node".into()
        });
    }
    if waiting.is_empty() {
        Ok(caught_up)
    } else {
        Err(waiting.join("; "))
    }
}

/// Wait until [`caught_up_since`] holds, printing progress when `progress` is set. Returns what
/// is still missing at the deadline.
async fn wait_for_caught_up(
    client: &Client,
    since: u128,
    timeout: Duration,
    progress: bool,
) -> Result<Result<CaughtUp, String>> {
    let started = std::time::Instant::now();
    let mut reported = None::<std::time::Instant>;
    let mut missing = "replication status did not answer".to_owned();
    loop {
        if let Ok(status) = client
            .get::<ReplicationStatus>("/v1/replication/status")
            .await
        {
            if let Some(removed) = &status.removed {
                anyhow::bail!("{}", removed.describe(status.fleet_id.as_deref()));
            }
            match caught_up_since(&status, since) {
                Ok(caught_up) => return Ok(Ok(caught_up)),
                Err(waiting) => missing = waiting,
            }
            if progress && reported.is_none_or(|at| at.elapsed() >= Duration::from_secs(10)) {
                reported = Some(std::time::Instant::now());
                println!("catching up: {missing}");
            }
        }
        if started.elapsed() >= timeout {
            return Ok(Err(missing));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn render_caught_up(caught_up: &CaughtUp, now: u128) -> String {
    let peers = caught_up
        .peers
        .iter()
        .map(|(peer, at)| format!("{peer} ({})", relative_time(*at, now)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut line = format!(
        "caught up now: at its latest exchange with {peers}, this node held every envelope the peer held"
    );
    if !caught_up.not_checked.is_empty() {
        line.push_str(&format!(
            "; not checked, since they are not up: {}",
            caught_up.not_checked.join(", ")
        ));
    }
    line
}

/// Print how a first sync ended, and fail when its graphs still differ.
fn report_first_sync(first: &st3::model::ReplicationFirstSync, json_output: bool) -> Result<()> {
    if json_output {
        print_value(first, true)?;
        anyhow::ensure!(first.state == "verified", "the first sync failed");
        return Ok(());
    }
    anyhow::ensure!(
        first.state == "verified",
        "{}\nThis node's views can be wrong. st replication status shows both graphs and the last \
         heal; report it with st diagnostic.",
        render_first_sync(first, now_ms())
    );
    println!("{}", render_first_sync(first, now_ms()));
    Ok(())
}

/// This node's first sync in one line.
fn render_first_sync(first: &st3::model::ReplicationFirstSync, now: u128) -> String {
    let peer = first.peer.as_deref().unwrap_or("its peer");
    let when = first
        .ended_at_unix_ms
        .map(|at| relative_time(at, now))
        .unwrap_or_default();
    let digests = format!(
        "this node {}, {peer} {}",
        short_digest(first.graph_digest.as_deref().unwrap_or("unknown")),
        short_digest(first.peer_graph_digest.as_deref().unwrap_or("unknown"))
    );
    match first.state.as_str() {
        "verified" if first.authority_digest.is_some() => format!(
            "first sync verified {when}: this node then held the same {} as {peer}; log digest {}; projection comparison waits for matching builds",
            envelope_count(first.envelopes.unwrap_or(0)),
            short_digest(first.authority_digest.as_deref().unwrap_or("unknown")),
        ),
        "verified" => format!(
            "first sync verified {when}: this node then held the same {} as {peer} and projected the same graph ({}){}",
            envelope_count(first.envelopes.unwrap_or(0)),
            short_digest(first.graph_digest.as_deref().unwrap_or("unknown")),
            if first.healed { ", after a heal" } else { "" }
        ),
        "failed" => format!(
            "first sync failed {when}: this node holds the same {} as {peer} but projects a different graph ({digests}), and a heal did not fix it: {}",
            envelope_count(first.envelopes.unwrap_or(0)),
            first.message.as_deref().unwrap_or("no reason recorded")
        ),
        _ => format!(
            "first sync from {peer} since {}",
            relative_time(first.started_at_unix_ms, now)
        ),
    }
}

/// The last heal with one peer in one line.
fn render_heal(peer: &str, report: &st3::model::ReplicationHealReport, now: u128) -> String {
    let mut moved = Vec::new();
    if report.refetched != 0 {
        moved.push(format!(
            "admitted {} claims {peer} projects",
            report.refetched
        ));
    }
    if report.pushed != 0 {
        moved.push(format!(
            "{peer} admitted {} claims this node projects",
            report.pushed
        ));
    }
    if report.replayed {
        moved.push("replayed this graph from nothing".into());
    }
    if report.peer_replayed {
        moved.push(format!("{peer} replayed its graph from nothing"));
    }
    let moved = if moved.is_empty() {
        String::new()
    } else {
        format!(": {}", moved.join(", "))
    };
    let narrowed = if report.ranges != 0 {
        format!(
            " ({} ranges and {} subjects differed)",
            report.ranges, report.subjects
        )
    } else {
        String::new()
    };
    if report.healed {
        format!(
            "healed {}{moved}{narrowed}; the graphs agree",
            relative_time(report.at_unix_ms, now)
        )
    } else {
        format!(
            "heal {}{moved}{narrowed} left the graphs different: {}",
            relative_time(report.at_unix_ms, now),
            report.unresolved.as_deref().unwrap_or("no reason recorded")
        )
    }
}

/// Each peer's line, then how far apart the two envelope sets are and how long catching up
/// should take, in words. Matching builds compare their graphs; mixed builds report the
/// equal envelope log and the pending projection comparison.
fn render_replication_peers(
    peers: &[ReplicationPeerStatus],
    local_graph_digest: &str,
    now: u128,
) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    for peer in peers
        .iter()
        .filter(|peer| peer.sync.as_ref().is_some_and(|sync| sync.diverged))
    {
        let sync = peer.sync.as_ref().expect("filtered on sync");
        let _ = writeln!(
            output,
            "sync\tdiverged: {} holds the same envelopes but projects a different graph, since {}",
            peer.peer,
            sync.graph_differs_since_unix_ms
                .map(|since| relative_time(since, now))
                .unwrap_or_else(|| "an unknown time".into())
        );
    }
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
            peer.refusal_reason
                .as_deref()
                .or(peer.last_error.as_deref())
                .unwrap_or("")
        );
        if !peer.differing_tables.is_empty() {
            let _ = writeln!(
                output,
                "  shared tables differ: {}",
                peer.differing_tables.join(", ")
            );
        }
        if let Some(at) = peer.last_success_at_unix_ms {
            let _ = writeln!(output, "  last seen {}", relative_time(at, now));
        } else {
            let _ = writeln!(output, "  no exchange yet");
        }
        if let Some(at) = peer.last_failure_at_unix_ms {
            let _ = writeln!(
                output,
                "  last attempt failed {}{}",
                relative_time(at, now),
                peer.last_error
                    .as_deref()
                    .map(|error| format!(": {error}"))
                    .unwrap_or_default()
            );
        }
        let Some(sync) = &peer.sync else {
            let _ = writeln!(output, "  difference not measured yet");
            continue;
        };
        if sync.stale {
            let _ = writeln!(
                output,
                "  stale: no exchange since the measurement below ({}), so it says what both held then, not now{}",
                relative_time(sync.measured_at_unix_ms, now),
                if sync.added_since_measured_envelopes == 0 {
                    String::new()
                } else {
                    format!(
                        "; this node has gained {} since",
                        envelope_count(sync.added_since_measured_envelopes)
                    )
                }
            );
        }
        if let Some(report) = &sync.heal {
            let _ = writeln!(output, "  {}", render_heal(&peer.peer, report, now));
        }
        let compared = sync
            .graph_compared_at_unix_ms
            .map(|at| relative_time(at, now))
            .unwrap_or_else(|| "never".into());
        let digests = if peer.projection_digests.is_empty() {
            format!(
                "legacy peer digest {}; full projection coverage unavailable",
                short_digest(peer.graph_digest.as_deref().unwrap_or("unknown"))
            )
        } else {
            format!(
                "this node {}, {} {}",
                short_digest(local_graph_digest),
                peer.peer,
                short_digest(peer.graph_digest.as_deref().unwrap_or("unknown"))
            )
        };
        if let Some(since) = sync.graph_differs_since_unix_ms {
            let _ = writeln!(
                output,
                "  {}: the same envelopes project different graphs since {} (compared {compared}; {digests})",
                if sync.diverged {
                    "diverged"
                } else {
                    "graphs differ"
                },
                relative_time(since, now)
            );
            if sync.diverged {
                let _ = writeln!(
                    output,
                    "  exchanges cannot fix this; the nodes heal by comparing the claims each projects, and views on one node are wrong until then"
                );
            } else {
                let _ = writeln!(
                    output,
                    "  diverged if this lasts a minute; a peer still projecting settles by itself"
                );
            }
        }
        if sync.peer_only_envelopes == 0 && sync.local_only_envelopes == 0 {
            if sync.graph_differs_since_unix_ms.is_some() {
                continue;
            }
            if peer.projection_comparison_waiting {
                let _ = writeln!(
                    output,
                    "  same envelopes (measured {}); projection comparison waits for a newer build",
                    relative_time(sync.measured_at_unix_ms, now)
                );
            } else if peer.graph_digest.as_deref() == Some(local_graph_digest) {
                let _ = writeln!(
                    output,
                    "  in sync: the same envelopes and the same graph (measured {})",
                    relative_time(sync.measured_at_unix_ms, now)
                );
            } else {
                let _ = writeln!(
                    output,
                    "  same envelopes (measured {}), but the graphs differ ({digests}); the next exchange compares them",
                    relative_time(sync.measured_at_unix_ms, now)
                );
            }
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

/// The first 12 characters of a digest, enough to tell two apart in a status line.
fn short_digest(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

async fn run_replication(
    client: &Client,
    config: &Config,
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
            if let Some(removed) = &status.removed {
                println!("removed\t{}", removed.describe(status.fleet_id.as_deref()));
            }
            println!("authority-digest\t{}", status.authority_digest);
            println!("graph-digest\t{}", status.graph_digest);
            for (table, digest) in &status.projection_digests {
                println!("table-digest\t{table}\t{digest}");
            }
            println!("envelopes\t{}", status.received_envelopes);
            println!(
                "records\tvalid={} pending={} waiting={} invalid={} repaired={} checkpointed={}",
                status.valid_records,
                status.pending_records,
                status.waiting_claims,
                status.invalid_records,
                status.repaired_records,
                status.checkpointed_envelopes
            );
            println!("unhealthy-projections\t{}", status.unhealthy_projections);
            if let Some(first) = &status.first_sync {
                println!("first-sync\t{}", render_first_sync(first, now_ms()));
            }
            for projection in &status.unhealthy {
                println!(
                    "unhealthy\t{}\t{}\t{}",
                    projection.aggregate,
                    projection.error_code.as_deref().unwrap_or(""),
                    projection.error_message.as_deref().unwrap_or("")
                );
            }
            let timings = &status.timings;
            println!(
                "timings\t{} exchanges, {} envelopes received; ms: round-trip={} export={} snapshot={} receipt={} admission={} (verify={}) projection={} repair={} signing={} sqlite={} ({} commits, {} ms)",
                timings.exchanges,
                timings.envelopes_received,
                timings.round_trip_ms,
                timings.export_ms,
                timings.snapshot_ms,
                timings.receipt_ms,
                timings.admission_ms,
                timings.verify_ms,
                timings.projection_ms,
                timings.repair_ms,
                timings.signing_ms,
                timings.sqlite_ms,
                timings.commits,
                timings.commit_ms
            );
            print!(
                "{}",
                render_replication_peers(&status.peers, &status.graph_digest, now_ms())
            );
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
                "differing_tables": remote.differing_tables,
                "graph": {
                    "local": status.graph_digest,
                    "remote": remote.graph_digest,
                    "equal": if remote.projection_digests.is_empty() { None } else { Some(remote.graph_digest.as_deref() == Some(status.graph_digest.as_str())) },
                    "coverage": if remote.projection_digests.is_empty() { "legacy-only" } else { "all-shared-projections" },
                },
            });
            if json_output {
                return print_value(&value, true);
            }
            println!("peer\t{}\t{}", peer, remote.status);
            if !remote.differing_tables.is_empty() {
                println!("different-tables\t{}", remote.differing_tables.join(", "));
            }
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
                if remote.projection_digests.is_empty() {
                    "unverified (legacy peer)"
                } else if remote.graph_digest.as_deref() == Some(status.graph_digest.as_str()) {
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
        ReplicationCommand::Checkpoint {
            command: CheckpointCommand::Plan { cut },
        } => {
            let request = st3::store::CheckpointPlanRequest { day: cut };
            let plan: st3::store::CheckpointPlanView =
                client.post("/v1/checkpoint/plan", &request).await?;
            if json_output {
                return print_value(&plan, true);
            }
            print!("{}", render_checkpoint_plan(&plan));
            Ok(())
        }
        ReplicationCommand::Checkpoint {
            command: CheckpointCommand::Status,
        } => {
            let status: st3::store::CheckpointStatusView =
                client.get("/v1/checkpoint/status").await?;
            if json_output {
                return print_value(&status, true);
            }
            print!("{}", render_checkpoint_status(&status));
            Ok(())
        }
        ReplicationCommand::Checkpoint {
            command:
                CheckpointCommand::Excuse {
                    writer,
                    reason,
                    actor,
                },
        } => {
            let actor = fleet_person(actor, config)?;
            let request = st3::store::CheckpointExcuseRequest {
                writer,
                reason,
                actor,
            };
            let claim: st3::model::ClaimRecord =
                client.post("/v1/checkpoint/excuse", &request).await?;
            print_value(&claim, json_output)
        }
        ReplicationCommand::Checkpoint {
            command: CheckpointCommand::Resume { reason, actor },
        } => {
            let actor = fleet_person(actor, config)?;
            let request = st3::store::CheckpointResumeRequest { reason, actor };
            let status: st3::store::CheckpointStatusView =
                client.post("/v1/checkpoint/resume", &request).await?;
            if json_output {
                return print_value(&status, true);
            }
            print!("{}", render_checkpoint_status(&status));
            Ok(())
        }
    }
}

fn render_checkpoint_status(status: &st3::store::CheckpointStatusView) -> String {
    let names = |names: &std::collections::BTreeSet<String>| {
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    };
    let mut output = format!("CHECKPOINTS  {}\n", status.node);
    match &status.newest_stable {
        Some(stable) => output.push_str(&format!(
            "stable        {} · {} participants\n",
            stable.checkpoint,
            stable.terms.participants.len()
        )),
        None => output.push_str("stable        none\n"),
    }
    output.push_str(&format!(
        "trimmed       {}\n",
        status.trimmed.as_deref().unwrap_or("none")
    ));
    if status.halted {
        output.push_str(
            "halted        a trim found the graph would change; see daemon diagnostics, then \
             `st replication checkpoint resume`\n",
        );
    }
    output.push_str(&format!("participants  {}\n", names(&status.participants)));
    if !status.excused.is_empty() {
        output.push_str(&format!("excused       {}\n", names(&status.excused)));
    }
    if !status.left.is_empty() {
        output.push_str(&format!("left          {}\n", names(&status.left)));
    }
    if let Some(pending) = &status.pending {
        output.push_str(&format!("pending       {}\n", pending.checkpoint));
        output.push_str(&format!("  sealed      {}\n", names(&pending.sealed)));
        output.push_str(&format!("  unsealed    {}\n", names(&pending.unsealed)));
        for (writer, difference) in &pending.disagreeing {
            output.push_str(&format!("  differs     {writer}: {difference}\n"));
        }
        output.push_str(&format!("  verified    {}\n", names(&pending.verified)));
        for (writer, difference) in &pending.verifications_differ {
            output.push_str(&format!("  verifies    {writer}: different {difference}\n"));
        }
        if !pending.verifications_agree {
            output.push_str(
                "  verifications disagree, so this checkpoint is not stable; the next due \
                 checkpoint tries again\n",
            );
        }
    }
    output
}

fn render_checkpoint_plan(plan: &st3::store::CheckpointPlanView) -> String {
    let mut output = String::new();
    let percent = |part: usize, whole: usize| {
        if whole == 0 {
            0.0
        } else {
            part as f64 * 100.0 / whole as f64
        }
    };
    output.push_str(&format!(
        "CHECKPOINT  {} · dry run, nothing changed\n",
        plan.checkpoint
    ));
    output.push_str(&format!(
        "before the cut  {} envelopes · {} claims\n",
        plan.sealed_envelopes, plan.sealed_claims
    ));
    output.push_str(&format!(
        "would drop      {} envelopes · {} claims ({:.1}%)\n",
        plan.dropped_envelopes,
        plan.dropped_claims,
        percent(plan.dropped_claims, plan.sealed_claims)
    ));
    for (kind, count) in &plan.by_kind {
        output.push_str(&format!(
            "  {kind}  {} of {}\n",
            count.dropped, count.sealed
        ));
    }
    let proof = &plan.proof;
    if proof.passed {
        output.push_str(&format!(
            "proof           passed · graph and {} subjects' readers unchanged\n",
            proof.subjects
        ));
    } else {
        output.push_str(&format!(
            "proof           FAILED · a checkpoint would not verify: {}\n",
            proof.mismatches.join(", ")
        ));
    }
    output.push_str(&format!("rules-digest    {}\n", plan.rules_digest));
    output.push_str(&format!("sealed-digest   {}\n", plan.sealed_digest));
    output.push_str(&format!("drop-digest     {}\n", plan.drop_digest));
    output.push_str(&format!("graph-digest    {}\n", proof.graph_digest));
    output
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
    // Product terminal IDs include the resource prefix; the private PTY routes and registry
    // tags name their underlying member. `terminals new` returns the public form.
    let subject = if namespace == "pty" {
        subject.strip_prefix("terminal/").unwrap_or(subject)
    } else {
        subject
    };
    if subject.contains('/') {
        subject.into()
    } else {
        format!("{namespace}/{subject}")
    }
}

/// The identity an attachment command acts as: the one named, the seat's, or the configured person.
fn blob_actor(named: Option<String>, configured_person: Option<&str>) -> Result<String> {
    let actor = named
        .or_else(|| std::env::var("ST_AGENT").ok().filter(|agent| !agent.is_empty()))
        .or_else(|| configured_person.map(str::to_owned))
        .context("name who acts with --as, or run inside a seat or with a configured person")?;
    reject_foreign_agent_actor(&actor)?;
    Ok(normalize_message_subject(&actor))
}

/// Upload a file as an attachment for `actor`, and name it for a message.
async fn upload_attachment(
    endpoint: &Endpoint,
    actor: &str,
    file: &Path,
    media_type: Option<&str>,
) -> Result<st3::model::AttachmentInput> {
    let metadata = fs::symlink_metadata(file)
        .with_context(|| format!("inspect attachment {}", file.display()))?;
    anyhow::ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        file.display()
    );
    anyhow::ensure!(
        metadata.len() <= st3::blobs::MAX_BLOB_BYTES as u64,
        "{} is larger than the {} MiB an attachment may be",
        file.display(),
        st3::blobs::MAX_BLOB_BYTES / (1024 * 1024)
    );
    let bytes = fs::read(file).with_context(|| format!("read attachment {}", file.display()))?;
    let media_type = media_type
        .map(str::to_owned)
        .or_else(|| st3::blobs::sniff_media_type(&bytes).map(str::to_owned))
        .with_context(|| {
            format!(
                "{} is not a PNG, JPEG, GIF or WebP image",
                file.display()
            )
        })?;
    let uploaded = generated_client(endpoint, Some(actor))?
        .upload_blob(bytes, &media_type)
        .await
        .map_err(|error| anyhow::anyhow!("{}", error.plain()))?
        .value;
    Ok(st3::model::AttachmentInput {
        blob: uploaded.blob,
        media_type: uploaded.media_type,
        name: file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
    })
}

async fn upload_attachments(
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

async fn run_blobs(
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    command: BlobCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        BlobCommand::Put {
            file,
            media_type,
            actor,
        } => {
            let actor = blob_actor(actor, configured_person)?;
            let attachment =
                upload_attachment(endpoint, &actor, &file, media_type.as_deref()).await?;
            if json_output {
                print_value(&attachment, true)
            } else {
                println!("{}", attachment.blob);
                Ok(())
            }
        }
        BlobCommand::Get {
            reference,
            message,
            output,
            actor,
        } => {
            let actor = blob_actor(actor, configured_person)?;
            let hash = st3::blobs::parse_reference(&reference)?;
            let bytes = generated_client(endpoint, Some(&actor))?
                .blob(&hash, message.as_deref())
                .await
                .map_err(|error| anyhow::anyhow!("{}", error.plain()))?;
            match output {
                Some(path) => {
                    fs::write(&path, &bytes)
                        .with_context(|| format!("write {}", path.display()))?;
                    println!("{}", path.display());
                }
                None => {
                    use std::io::Write as _;
                    std::io::stdout().write_all(&bytes)?;
                }
            }
            Ok(())
        }
    }
}

async fn run_rules(
    client: &Client,
    config: &Config,
    command: RuleCommand,
    json_output: bool,
) -> Result<()> {
    use smallclaims::rules::{Mode, NamedRule};
    let person = |actor: Option<String>| -> Result<String> {
        actor
            .or_else(|| config.person.clone())
            .context("rules are set by a person: set person in config.toml or pass --as person/NAME")
    };
    let set = |actor: &str, name: &str, rule: smallclaims::rules::Rule| {
        let request = st3::api::RuleSetRequest {
            actor: actor.to_owned(),
            name: name.to_owned(),
            rule,
        };
        async move {
            let _: Value = client.post("/v1/rules/set", &request).await?;
            anyhow::Ok(())
        }
    };
    match command {
        RuleCommand::Ls => {
            let rules: Vec<NamedRule> = client.get("/v1/rules").await?;
            if json_output {
                return print_value(&rules, true);
            }
            if rules.is_empty() {
                println!("no rules: every principal may write what its person may (st rules lockdown sets the presets)");
            }
            for rule in rules {
                println!(
                    "{}\t{}\t{}",
                    rule.name,
                    rule.rule.mode.as_str(),
                    rule.rule.description
                );
            }
            Ok(())
        }
        RuleCommand::Audit { rule, limit } => {
            let mut path = format!("/v1/rules/audit?limit={limit}");
            if let Some(rule) = &rule {
                path.push_str(&format!("&rule={}", urlencoding::encode(rule)));
            }
            let audits: Vec<st3::api::RuleAudit> = client.get(&path).await?;
            if json_output {
                return print_value(&audits, true);
            }
            if audits.is_empty() {
                println!("no write has been refused or audited");
            }
            for audit in audits {
                println!(
                    "{}\t{}\t{} {} on {}",
                    audit.at_unix_ms, audit.rule, audit.actor, audit.action, audit.target
                );
            }
            Ok(())
        }
        RuleCommand::Lockdown { actor, starters } => {
            let actor = person(actor)?;
            let rules = st3::rules::lockdown(&starters);
            for (name, rule) in &rules {
                set(&actor, name, rule.clone()).await?;
            }
            if json_output {
                return print_value(
                    &rules
                        .iter()
                        .map(|(name, rule)| json!({"name": name, "rule": rule}))
                        .collect::<Vec<_>>(),
                    true,
                );
            }
            for (name, rule) in &rules {
                println!("{name}\taudit\t{}", rule.description);
            }
            println!(
                "Each rule logs what it would refuse; read the log with st rules audit, then st rules mode NAME enforce."
            );
            Ok(())
        }
        RuleCommand::Mode { name, mode, actor } => {
            let actor = person(actor)?;
            let rules: Vec<NamedRule> = client.get("/v1/rules").await?;
            let mut rule = rules
                .into_iter()
                .find(|rule| rule.name == name)
                .with_context(|| format!("no rule is named `{name}`; st rules ls lists them"))?
                .rule;
            rule.mode = match mode.as_str() {
                "off" => Mode::Off,
                "enforce" => Mode::Enforce,
                _ => Mode::Audit,
            };
            set(&actor, &name, rule).await?;
            if json_output {
                return print_value(&json!({"name": name, "mode": mode}), true);
            }
            println!("{name}\t{mode}");
            Ok(())
        }
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
                let (partial, shown) = (response.has_more, response.items.len());
                print_value(&response, true)?;
                if partial {
                    st3::gate_report::note_partial_listing(shown);
                }
                Ok(())
            } else {
                let shown = response.items.len();
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
                if response.has_more {
                    if let Some(cursor) = response.next_cursor {
                        println!(
                            "More document versions are available. Continue with: {}",
                            document_continuation_command(name.as_deref(), all, limit, &cursor)
                        );
                    }
                    st3::gate_report::note_partial_listing(shown);
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
                print_value(&response, true)?;
                note_partial_page(&response.value);
                return Ok(());
            }
            let partial = response.value.page.has_more;
            let shown = response.value.items.len();
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
            if partial {
                st3::gate_report::note_partial_listing(shown);
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
            let client = cli_client(endpoint);
            let (subject, tokens, existing, mission) =
                agent_start_declaration(&client, &args).await?;
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
            print_value(&response, json_output)?;
            if !json_output
                && let Some(subject) = response
                    .subject_tokens
                    .keys()
                    .find(|subject| subject.starts_with("agent/"))
            {
                let agent = generated_client(endpoint, None)?
                    .agents_get(subject)
                    .await?;
                if let ClientResource::Agent(agent) = agent.value {
                    let state = cli_help::agent_state(
                        &agent.state,
                        agent.harness_state.as_deref(),
                        agent.fault.as_deref(),
                        &agent.reachability,
                    );
                    print!(
                        "{}",
                        cli_help::agent_next_steps(subject, &args.actor, &state)
                    );
                }
            }
            Ok(())
        }
        AgentsCommand::Suspend(args) => {
            let subject = format!("agent/{}", args.subject);
            let agent = request_suspension(
                endpoint,
                "/v1/agents/suspend",
                &subject,
                &args.actor,
                args.reason.as_deref(),
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
                    }),
                )
                .await?;
            let previous = request.body["fields"]["incarnation_id"]
                .as_str()
                .unwrap_or("");
            let gateway = generated_client(endpoint, None)?;
            let wait = async {
                let mut cursor = request.store_index;
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
                        if let Some(fault) = agent.fault.as_deref() {
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
                    let events: Vec<EventRecord> = client
                        .get(&format!(
                            "/v1/events?after={cursor}&subject={}&wait=true&timeout_ms=1000",
                            urlencoding::encode(&subject),
                        ))
                        .await?;
                    for event in events {
                        let fields = event.body.get("fields").unwrap_or(&event.body);
                        if event.kind == "runtime.reconcile-decision"
                            && matches!(fields["decision"].as_str(), Some("member-fault" | "raise"))
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

fn parse_agent_start_identity(identity: &str) -> Result<String, String> {
    if identity.starts_with("agent/agent/") {
        return Err(
            "doubled agent/agent/ prefix; accepted forms are ID or agent/ID \
             (for example, example/worker or agent/example/worker)"
                .into(),
        );
    }
    Ok(identity.strip_prefix("agent/").unwrap_or(identity).into())
}

/// What `st agents start` found for a seat its mission run declared.
enum MissionSeatStart {
    /// The run still declares it; the run's mission run subject.
    Declared(String),
    /// Someone stopped it; the daemon restores the run's declaration.
    Stopped,
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
            "`{subject}` has no unambiguous prior agent declaration; use `st agents apply`"
        );
        claim = client
            .get(&format!("/v1/claims/by-id/{}", claim.predecessors[0]))
            .await?;
    }
}

fn declaration_node(value: &Value) -> Result<KdlNode> {
    let mut node = KdlNode::new(value["name"].as_str().context("declaration has no name")?);
    if let Some(arguments) = value["arguments"].as_array() {
        for argument in arguments {
            node.entries_mut()
                .push(KdlEntry::new(declaration_value(argument)?));
        }
    }
    if let Some(properties) = value["properties"].as_object() {
        for (name, value) in properties {
            node.entries_mut()
                .push(KdlEntry::new_prop(name.as_str(), declaration_value(value)?));
        }
    }
    if let Some(children) = value["children"].as_array() {
        let mut body = KdlDocument::new();
        for child in children {
            body.nodes_mut().push(declaration_node(child)?);
        }
        node.set_children(body);
    }
    Ok(node)
}

fn declaration_value(value: &Value) -> Result<kdl::KdlValue> {
    Ok(match value {
        Value::String(value) => kdl::KdlValue::String(value.clone()),
        Value::Bool(value) => kdl::KdlValue::Bool(*value),
        Value::Null => kdl::KdlValue::Null,
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                kdl::KdlValue::Integer(integer.into())
            } else {
                kdl::KdlValue::Float(value.as_f64().context("invalid declaration number")?)
            }
        }
        _ => anyhow::bail!("invalid declaration value: {value}"),
    })
}

fn replace_declaration_child(body: &mut KdlDocument, node: KdlNode) {
    if let Some(current) = body
        .nodes_mut()
        .iter_mut()
        .find(|child| child.name() == node.name())
    {
        *current = node;
    } else {
        body.nodes_mut().push(node);
    }
}

fn agent_start_document(
    args: &AgentStartArgs,
    existing: Option<&st3::model::DesiredSubject>,
) -> Result<String> {
    let mut agent = match existing {
        Some(existing) => declaration_node(&existing.desired)?,
        None => kdl_node("agent", [args.identity.as_str()]),
    };
    let mut body = agent.children_mut().take().unwrap_or_default();
    if let Some(existing) = existing {
        // Pin the existing subject before changing placement, including a host-prefixed simple
        // name and a declaration originally nested inside a host.
        let identity = existing
            .subject
            .strip_prefix("agent/")
            .unwrap_or(&existing.subject);
        if body.get("identity").is_some() {
            replace_declaration_child(&mut body, kdl_node("identity", [identity]));
        } else {
            agent.entries_mut()[0] = KdlEntry::new(identity);
        }
    }
    if let Some(host) = args.host.as_deref().or_else(|| {
        existing.and_then(|seat| seat.member.as_ref().map(|member| member.host.as_str()))
    }) {
        replace_declaration_child(&mut body, kdl_node("host", [host]));
    }
    if args.workspace.is_some() || existing.is_none() {
        let path = args.workspace.as_deref().unwrap_or_else(|| Path::new("."));
        let workspace = fs::canonicalize(path)
            .with_context(|| format!("resolve workspace {}", path.display()))?
            .display()
            .to_string();
        replace_declaration_child(&mut body, kdl_node("workspace", [workspace.as_str()]));
    }
    if existing.is_none() {
        body.nodes_mut().push(kdl_node("restart", ["always"]));
    }
    let harness_options =
        args.model.is_some() || args.effort.is_some() || !args.arguments.is_empty();
    if existing.is_some() && body.get("harness").is_none() && harness_options {
        let style = if body.get("command").is_some() {
            "command"
        } else {
            "argv"
        };
        anyhow::bail!(
            "`{}` uses a `{style}` declaration, not a typed harness; \
             --model, --effort and --arg require a typed harness",
            args.identity
        );
    }
    if existing.is_none() || body.get("harness").is_some() || args.harness.is_some() {
        if args.harness.is_some() {
            body.nodes_mut()
                .retain(|node| !matches!(node.name().value(), "command" | "argv"));
        }
        if body.get("harness").is_none() {
            body.nodes_mut().push(kdl_node(
                "harness",
                [args.harness.as_deref().unwrap_or("claude")],
            ));
        }
        let harness = body.get_mut("harness").expect("harness was inserted");
        if let Some(driver) = &args.harness {
            harness.entries_mut()[0] = KdlEntry::new(driver.as_str());
        }
        let harness_body = harness.ensure_children();
        for (name, value) in [("model", &args.model), ("effort", &args.effort)] {
            if let Some(value) = value {
                replace_declaration_child(harness_body, kdl_node(name, [value.as_str()]));
            }
        }
        if !args.arguments.is_empty() {
            replace_declaration_child(
                harness_body,
                kdl_node("args", args.arguments.iter().map(String::as_str)),
            );
        }
    }
    agent.set_children(body);
    Ok(publication_document(agent))
}

#[cfg(test)]
use st3::creation::CLAUDE_SEAT_SETTINGS;

fn agent_new_document(args: &AgentNewArgs, workspace: &str, create_workspace: bool) -> String {
    let parameters = st3_client::AgentCreateParameters {
        name: args.name.clone(),
        harness: args.harness.clone(),
        host: args.host.clone(),
        model: args.model.clone(),
        effort: args.effort.clone(),
        description: args.description.clone(),
        workspace: None,
        message: args.message.clone(),
    };
    let key = args
        .message
        .as_ref()
        .map(|_| uuid::Uuid::now_v7().to_string());
    st3::creation::agent_document(&parameters, workspace, create_workspace, key.as_deref())
}

async fn run_agent_new(
    endpoint: &Endpoint,
    configured_person: Option<&str>,
    args: AgentNewArgs,
    json_output: bool,
) -> Result<()> {
    let client = cli_client(endpoint);
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
                    "`{subject}` already exists and is {}; change it with `st agents apply`, or stop it with `st agents stop` first",
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
        attach_terminal(&client, endpoint, None, person, &subject, false).await?;
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
        let events: Vec<EventRecord> = client
            .get(&format!(
                "/v1/events?after={cursor}&subject={}&wait=true&timeout_ms=30000",
                urlencoding::encode(subject)
            ))
            .await?;
        for event in events {
            cursor = cursor.max(event.store_index);
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
        AgentsCommand::New(_)
        | AgentsCommand::Apply(_)
        | AgentsCommand::Start(_)
        | AgentsCommand::Stop(_)
        | AgentsCommand::Restart(_)
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

/// A lane change is made by `--as`, else by the harness's own seat, else by the configured person.
fn lane_actor(explicit: Option<&str>, configured_person: Option<&str>) -> Result<String> {
    if let Some(actor) = explicit {
        return Ok(actor.to_owned());
    }
    if let Some(own) = std::env::var("ST_AGENT")
        .ok()
        .map(|own| own.trim().to_owned())
        .filter(|own| !own.is_empty())
    {
        return Ok(seat_subject(&own));
    }
    configured_person.map(str::to_owned).context(
        "st lanes needs `--as person/NAME`, `--as agent/PATH`, or `person = \"person/NAME\"` in the st config",
    )
}

async fn run_lanes(
    client: &Client,
    configured_person: Option<&str>,
    command: LaneCommand,
    json_output: bool,
) -> Result<()> {
    let change =
        |lane: String, change: &str, entry: String, actor: String| st3::model::LaneChangeRequest {
            lane,
            change: change.into(),
            entry,
            reason: None,
            outcome: None,
            placement: None,
            anchor: None,
            state: None,
            detail: None,
            head: None,
            actor,
            idempotency_key: format!("lane-change:{}", uuid::Uuid::now_v7().simple()),
        };
    let request = match command {
        LaneCommand::Ls { all } => {
            let lanes: Vec<st3::model::LaneView> =
                client.get(&format!("/v1/lanes?all={all}")).await?;
            if json_output {
                return print_value(&lanes, true);
            }
            print!("{}", render_lanes(&lanes));
            return Ok(());
        }
        LaneCommand::Show { lane } => {
            let lane: st3::model::LaneView = client
                .get(&format!("/v1/lanes/{}", urlencoding::encode(&lane)))
                .await?;
            if json_output {
                return print_value(&lane, true);
            }
            print!("{}", render_lane(&lane, current_unix_ms()?));
            return Ok(());
        }
        LaneCommand::Join(args) => {
            let actor = lane_actor(args.actor.as_deref(), configured_person)?;
            let mut request = change(args.lane, "join", args.entry, actor);
            request.reason = args.reason;
            request
        }
        LaneCommand::Approve(args) => {
            let actor = lane_actor(args.actor.as_deref(), configured_person)?;
            let mut request = change(args.lane, "approve", args.entry, actor);
            request.reason = args.reason;
            request
        }
        LaneCommand::Leave(args) => {
            let actor = lane_actor(args.entry.actor.as_deref(), configured_person)?;
            let mut request = change(args.entry.lane, "leave", args.entry.entry, actor);
            request.reason = args.entry.reason;
            request.outcome = Some(args.outcome);
            request
        }
        LaneCommand::Move(args) => {
            let actor = lane_actor(args.entry.actor.as_deref(), configured_person)?;
            let mut request = change(args.entry.lane, "move", args.entry.entry, actor);
            request.reason = args.entry.reason;
            let (placement, anchor) = if args.top {
                ("top", None)
            } else if args.bottom {
                ("bottom", None)
            } else if let Some(before) = args.before {
                ("before", Some(before))
            } else if let Some(after) = args.after {
                ("after", Some(after))
            } else {
                anyhow::bail!("choose one of --top, --bottom, --before ENTRY, or --after ENTRY");
            };
            request.placement = Some(placement.into());
            request.anchor = anchor;
            request
        }
        LaneCommand::Mark(args) => {
            let actor = lane_actor(args.actor.as_deref(), configured_person)?;
            let mut request = change(args.lane, "mark", args.entry, actor);
            request.state = Some(args.state);
            request.detail = args.detail;
            request.head = args.head;
            request
        }
    };
    let response: st3::model::LaneChangeResponse =
        client.post("/v1/lane-changes", &request).await?;
    if json_output {
        return print_value(&response, true);
    }
    let lane = &response.lane;
    let entry = st3::lane::entry_subject(lane.entries_prefix.as_deref(), &request.entry);
    let short = st3::lane::short_entry(lane.entries_prefix.as_deref(), &entry);
    let summary = match (request.change.as_str(), response.claim.is_some()) {
        ("join", false) => format!("{short} is already in {}", lane.subject),
        ("join", true) => format!("{short} joined {}", lane.subject),
        ("leave", _) => format!("{short} left {}", lane.subject),
        ("move", _) => format!("moved {short} in {}", lane.subject),
        ("mark", _) => format!("marked {short} {}", request.state.as_deref().unwrap_or("")),
        ("approve", _) => format!("approved {short} in {}", lane.subject),
        (other, _) => format!("{other} {short}"),
    };
    println!("{summary}");
    print!("{}", render_lane(lane, current_unix_ms()?));
    Ok(())
}

fn render_lanes(lanes: &[st3::model::LaneView]) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "LANES  {}", lanes.len());
    if lanes.is_empty() {
        let _ = writeln!(output, "  No mission run declares an open lane.");
    }
    for lane in lanes {
        let prefix = lane.entries_prefix.as_deref();
        let front = lane.entries.first().map(|entry| {
            format!(
                " · front {} {}",
                st3::lane::short_entry(prefix, &entry.entry),
                entry.state
            )
        });
        let _ = writeln!(
            output,
            "  {}  {} {}{}{}",
            lane.subject,
            lane.entries.len(),
            if lane.entries.len() == 1 {
                "entry"
            } else {
                "entries"
            },
            front.unwrap_or_default(),
            if lane.open { "" } else { " · closed" }
        );
    }
    output
}

/// One lane entry on one line: position, short entry, status, detail, and who joined it.
fn render_lane_entry(
    output: &mut String,
    indent: &str,
    prefix: Option<&str>,
    entry: &st3::lane::Entry,
    now: u128,
) {
    use std::fmt::Write as _;

    let _ = write!(
        output,
        "{indent}{}. {}  {}",
        entry.position,
        st3::lane::short_entry(prefix, &entry.entry),
        entry.state
    );
    if let Some(detail) = entry.detail.as_deref() {
        let _ = write!(output, "  {detail}");
    }
    let _ = write!(
        output,
        "  joined by {} {}",
        entry.joined_by,
        presentation::relative_time(entry.joined_at_unix_ms, now)
    );
    if let Some(approver) = entry.approved_by.as_deref() {
        let _ = write!(output, ", approved by {approver}");
    }
    let _ = writeln!(output);
}

fn render_lane(lane: &st3::model::LaneView, now: u128) -> String {
    use std::fmt::Write as _;

    let prefix = lane.entries_prefix.as_deref();
    let mut output = String::new();
    let _ = writeln!(output, "LANE      {}", lane.subject);
    if let Some(run) = lane.run.as_deref() {
        let _ = writeln!(output, "RUN       {run}");
    }
    if !lane.open {
        let _ = writeln!(
            output,
            "STATE     closed: its run ended or a revision dropped it"
        );
    }
    if let Some(prefix) = prefix {
        let _ = writeln!(output, "ENTRIES   {prefix}");
    }
    if let Some(approver) = lane.approver.as_deref() {
        let _ = writeln!(output, "APPROVER  {approver}");
    }
    let _ = writeln!(output, "QUEUE     {}", lane.entries.len());
    if lane.entries.is_empty() {
        let _ = writeln!(output, "  The lane is empty.");
    }
    for entry in &lane.entries {
        render_lane_entry(&mut output, "  ", prefix, entry, now);
    }
    if !lane.recent.is_empty() {
        let _ = writeln!(output, "RECENT");
    }
    for recent in &lane.recent {
        let short = st3::lane::short_entry(prefix, &recent.entry);
        let change = match recent.kind.as_str() {
            "left" => format!(
                "{short} left ({})",
                recent.outcome.as_deref().unwrap_or("removed")
            ),
            "moved" => match (recent.placement.as_deref(), recent.anchor.as_deref()) {
                (Some("top"), _) => format!("moved {short} to the top"),
                (Some("bottom"), _) => format!("moved {short} to the bottom"),
                (Some(placement), Some(anchor)) => format!(
                    "moved {short} {placement} {}",
                    st3::lane::short_entry(prefix, anchor)
                ),
                _ => format!("moved {short}"),
            },
            kind => format!("{kind} {short}"),
        };
        let _ = write!(
            output,
            "  {change} by {} {}",
            recent.actor,
            presentation::relative_time(recent.at_unix_ms, now)
        );
        if let Some(reason) = recent.reason.as_deref() {
            let _ = write!(output, ": {reason}");
        }
        let _ = writeln!(output);
    }
    output
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
    // A daemon without lanes sends no `lanes` field; show the section only when it does.
    if let Some(lanes) = value["lanes"].as_array() {
        output.push_str("LANES\n");
        if lanes.is_empty() {
            output.push_str("  none\n");
        }
        for lane in lanes {
            let entries = lane["entries"].as_array().map_or(0, Vec::len);
            let _ = write!(
                output,
                "  {}  {entries} {}",
                lane["id"].as_str().unwrap_or("unknown"),
                if entries == 1 { "entry" } else { "entries" }
            );
            if let Some(front) = lane["entries"].get(0) {
                let _ = write!(
                    output,
                    " · front {} {}",
                    front["label"].as_str().unwrap_or("unknown"),
                    front["state"].as_str().unwrap_or("unknown")
                );
            }
            output.push('\n');
        }
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

/// Provider text is data, never terminal commands or additional output lines.
fn push_todo_terminal_text(output: &mut String, text: &str) {
    let mut characters = text.chars();
    let mut space = false;
    let mut written = false;
    while let Some(character) = characters.next() {
        if matches!(character, '\u{1b}' | '\u{9b}' | '\u{9d}') {
            let introducer = if character == '\u{1b}' {
                characters.next()
            } else if character == '\u{9b}' {
                Some('[')
            } else {
                Some(']')
            };
            match introducer {
                Some('[') => {
                    for next in characters.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) { break; }
                    }
                }
                Some(']') => {
                    while let Some(next) = characters.next() {
                        if next == '\u{7}' || (next == '\u{1b}' && characters.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if character.is_whitespace() {
            space = written;
        } else if !character.is_control() {
            if space { output.push(' '); }
            output.push(character);
            written = true;
            space = false;
        }
    }
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
    if let Some(todo) = &agent.todo {
        let snapshot = &todo.snapshot;
        let _ = write!(output, "Todo         ");
        if let Some(active) = snapshot.phases.iter().flat_map(|phase| &phase.tasks)
            .find(|task| task.status == st3_client::HarnessTaskStatus::InProgress)
        {
            output.push_str("▶ ");
            push_todo_terminal_text(&mut output, &active.content);
            output.push_str(" · ");
        }
        let totals = &snapshot.totals;
        let total = u128::from(totals.pending) + u128::from(totals.in_progress)
            + u128::from(totals.completed) + u128::from(totals.blocked);
        let _ = write!(output, "{}/{total} done · {} blocked", totals.completed, totals.blocked);
        if totals.abandoned > 0 {
            let _ = write!(output, " · {} abandoned", totals.abandoned);
        }
        if snapshot.truncated {
            output.push_str(" · truncated");
        }
        if todo.stale {
            output.push_str(" · stale");
        }
        output.push('\n');
    }

    if let Some(fault) = &agent.fault {
        let _ = writeln!(output, "FAULT        {fault}");
    }
    if let Some(suspension) = &agent.suspension {
        let session = match (&suspension.harness, &suspension.native_session_id) {
            (Some(harness), Some(session)) => format!(" · {harness} session {session}"),
            (None, Some(session)) => format!(" · session {session}"),
            _ => String::new(),
        };
        let failure = match (&suspension.code, &suspension.reason) {
            (Some(code), Some(reason)) => format!(" · {code}: {reason}"),
            (Some(code), None) => format!(" · {code}"),
            _ => String::new(),
        };
        let _ = writeln!(
            output,
            "SUSPENSION   {} {}{session}{failure}",
            suspension.action, suspension.phase
        );
    }
    if let Some(delivery) = &agent.delivery {
        match delivery.reason.as_deref() {
            Some(reason) => {
                let _ = writeln!(output, "DELIVERY     {} · {reason}", delivery.state);
            }
            None => {
                let _ = writeln!(output, "DELIVERY     {}", delivery.state);
            }
        }
    }
    if let Some(incarnation) = &agent.incarnation_id {
        let _ = writeln!(output, "INCARNATION  {incarnation}");
    }
    if let Some(owner) = &agent.owner_run_id {
        let _ = writeln!(output, "MISSION      {owner}");
    }
    if let Some(usage) = &agent.usage {
        let _ = writeln!(output, "USAGE        {}", render_usage(usage));
        if usage.incarnation_count > 0 {
            let _ = writeln!(
                output,
                "TOKENS       input {} · output {} · cache write {} · cache read {}",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_write_tokens,
                usage.cached_tokens
            );
        }
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
    for subagent in &agent.subagents {
        let _ = writeln!(
            output,
            "SUBAGENT     {}",
            subagent_line(subagent, Some(now_unix_ms))
        );
    }
    for runtime in &agent.runtime_ids {
        let _ = writeln!(output, "RUNTIME      {runtime}");
    }
    output
}

/// One line naming a running subagent: what it does, its type, and, given the time, when it
/// started.
fn subagent_line(subagent: &st3_client::AgentSubagent, now_unix_ms: Option<u128>) -> String {
    let mut parts = vec![
        subagent
            .description
            .clone()
            .unwrap_or_else(|| subagent.id.clone()),
    ];
    if let Some(kind) = &subagent.subagent_type {
        parts.push(kind.clone());
    }
    if let (Some(now), Some(started)) = (
        now_unix_ms,
        subagent
            .started_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok()),
    ) {
        let started = u128::try_from(started.timestamp_millis()).unwrap_or(0);
        parts.push(format!("started {}", relative_time(started, now)));
    }
    parts.join(" · ")
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
                // The subagents its harness runs now are its children.
                for (index, subagent) in agent.subagents.iter().enumerate() {
                    let branch = if index + 1 == agent.subagents.len() {
                        "└─"
                    } else {
                        "├─"
                    };
                    let _ = writeln!(
                        output,
                        "{member_prefix}   {branch} {}",
                        subagent_line(subagent, None)
                    );
                }
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
            watch,
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
            if watch {
                return run_collection_watch(
                    endpoint,
                    Some(&actor),
                    "attention",
                    None,
                    None,
                    limit,
                    &format!("HUMAN ATTENTION FOR {actor}"),
                    json_output,
                )
                .await;
            }
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

const STRUCTURED_REQUEST_HELP: &str = r##"A structured request as a JSON file (`-` reads stdin).

A `decision` proposes one action: exactly one `accept` answer, one `decline` answer and at most
one `request_changes` answer, each naming what happens next. A `choice` names 2 to 5 options,
and `"custom": true` also takes the person's own words. `feedback` asks for text and has no
answers. `why_person` says why no runtime fact or standing instruction settles it. Omit
`recommendation` to make none. An `update` asks nothing: it names `about`, the person's own
run or step or their message to you, and clears once read (`st work update` builds one). Subject kinds: pull_request, issue, document, mission, run,
step, agent, host, commit, link; `revision` pins what was reviewed.

  {"version": 1, "type": "decision",
   "question": "Land #11 then #12?",
   "why_person": "The owner approves merges to the public repository.",
   "reasons": ["Checks are green on both heads."],
   "recommendation": {"answer": "land", "reason": "Both are reviewed."},
   "subjects": [{"kind": "pull_request", "label": "#11",
                 "url": "https://github.com/OWNER/REPO/pull/11", "revision": "HEAD_SHA"}],
   "answers": [
     {"id": "land", "label": "Land #11 then #12", "outcome": "accept",
      "consequence": "I queue #11, then #12."},
     {"id": "keep-open", "label": "Keep both open", "outcome": "decline",
      "consequence": "Nothing merges."},
     {"id": "revise", "label": "Request changes", "outcome": "request_changes",
      "consequence": "I make the changes and ask again."}]}

The person answers with `st work done STEP --answer ID [--text TEXT]`; requesting changes,
feedback and a custom choice need text. The resumed step's `person_answers` carries the answer
as {"type", "outcome", "id", "label", "text"}: read it with `st work show STEP --json` and
act on `id` and `outcome`, not on the words."##;

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

async fn run_work(
    client: &Client,
    endpoint: &Endpoint,
    command: WorkCommand,
    json_output: bool,
) -> Result<()> {
    match command {
        WorkCommand::Ask(args) => {
            reject_foreign_agent_actor(&args.actor)?;
            let request = args
                .request
                .as_deref()
                .map(read_structured_request)
                .transpose()?;
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
    for response in &work.person_answers {
        let typed = response
            .answer
            .as_ref()
            .map(|answer| {
                let id = answer
                    .id
                    .as_deref()
                    .map(|id| format!(" {id}"))
                    .unwrap_or_default();
                format!("{}{id}", answer.outcome)
            })
            .unwrap_or_else(|| response.status.clone());
        let _ = writeln!(
            output,
            "Person answer: {typed}: {} ({}, {})",
            response.summary, response.respondent, response.ask
        );
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
    message_page_reporting(client, recipient, include_closed, cursor, None).await
}

/// One mailbox page. A seat's delivery process passes its delivery report, which the daemon
/// keeps in memory to tell a live, current delivery path from a stale one.
async fn message_page_reporting(
    client: &Client,
    recipient: Option<&str>,
    include_closed: bool,
    cursor: Option<&str>,
    report: Option<&str>,
) -> Result<MessagePage> {
    let mut path = format!("/v1/messages/page?include_closed={include_closed}&limit=100");
    if let Some(recipient) = recipient {
        path.push_str(&format!("&to={}", urlencoding::encode(recipient)));
    }
    if let Some(cursor) = cursor {
        path.push_str(&format!("&cursor={}", urlencoding::encode(cursor)));
    }
    if let Some(report) = report {
        path.push_str(&format!("&delivery={}", urlencoding::encode(report)));
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

async fn send_message(
    client: &Client,
    args: MessageSendArgs,
    attachments: Vec<st3::model::AttachmentInput>,
) -> Result<Option<MessageSendReceipt>> {
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
    let mut request = MessageSendRequest {
        idempotency_key: String::new(),
        from,
        to,
        content: args.body,
        title: args.subject,
        in_reply_to: args.in_reply_to,
        tags: args.tags,
        attachments,
    };
    request.idempotency_key = match args.idempotency_key {
        Some(key) => key,
        None => {
            let incarnation = std::env::var("ST3_INCARNATION")
                .ok()
                .filter(|incarnation| !incarnation.is_empty());
            let hour = message_key_hour();
            let key = derived_message_key(&request, incarnation.as_deref(), hour);
            // A send whose answer was lost just before the hour turned landed under the
            // previous hour's key.
            if let Some(previous) = hour.checked_sub(1) {
                let previous = derived_message_key(&request, incarnation.as_deref(), previous);
                match client.sent_message(&previous).await {
                    Ok(Some(receipt)) => return Ok(Some(receipt)),
                    Ok(None) => {}
                    // A daemon from before the lookup has no such route. Its sends still repeat
                    // only within the hour.
                    Err(error) if st3::client::http_status(&error) == Some(404) => {}
                    // A busy daemon still takes the send: only a send whose answer was lost in
                    // the hour's last moments could repeat, and refusing every send is worse.
                    Err(error) => eprintln!(
                        "st: could not check the previous hour for this message ({}); sending it with idempotency key {key}",
                        plain_error(&error)
                    ),
                }
            }
            key
        }
    };
    match client.send_message(&request).await {
        Ok(receipt) => Ok(Some(receipt)),
        Err(error) => Err(message_send_error(error, &request.idempotency_key)),
    }
}

/// The prefix of a key a send derives from its message. The version changes with what goes in.
const DERIVED_MESSAGE_KEY_PREFIX: &str = "st3-message:v1:";

/// The UTC hour a derived message key belongs to. `ST3_MESSAGE_KEY_HOUR` stands in for the
/// clock, so a test can send an hour later without waiting one.
fn message_key_hour() -> u64 {
    std::env::var("ST3_MESSAGE_KEY_HOUR")
        .ok()
        .and_then(|hour| hour.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                / 3600
        })
}

/// A send without `--idempotency-key` is named by what it sends: the sender, recipient, the
/// message it answers, title, words, tags and attachments, the seat incarnation that sends it,
/// and the hour. Running the same command again in that hour names the same message, so the
/// daemon returns it instead of sending a second; the same words a few hours later are new.
fn derived_message_key(
    request: &MessageSendRequest,
    incarnation: Option<&str>,
    hour: u64,
) -> String {
    let attachments = request
        .attachments
        .iter()
        .map(|attachment| json!([attachment.blob, attachment.media_type, attachment.name]))
        .collect::<Vec<_>>();
    let canonical = json!([
        DERIVED_MESSAGE_KEY_PREFIX,
        request.from,
        request.to,
        request.in_reply_to,
        request.title,
        request.content,
        request.tags,
        attachments,
        incarnation,
        hour,
    ]);
    let digest = hex::encode(Sha256::digest(canonical.to_string().as_bytes()));
    format!("{DERIVED_MESSAGE_KEY_PREFIX}{}", &digest[..32])
}

/// Say what a failed send means for the sender: its key, how to tell whether it landed, and that
/// running the same command again sends it at most once.
fn message_send_error(error: anyhow::Error, key: &str) -> anyhow::Error {
    let check = format!(
        "st conversations status --idempotency-key {}",
        shell_word(key)
    );
    let unconfirmed = format!(
        "st did not confirm the message, so it may or may not have been sent. Check with `{check}`; running the same command again is safe and sends it at most once"
    );
    if let Some(error) = error.downcast_ref::<st3::client::MessageSendUnconfirmed>() {
        return anyhow::anyhow!("{unconfirmed}. st did not answer: {}", error.reason());
    }
    // A refusal is an answer: the daemon wrote nothing for this key.
    if st3::client::http_status(&error).is_some_and(|status| (400..500).contains(&status)) {
        return error.context(format!("st refused the message (idempotency key {key})"));
    }
    error.context(unconfirmed)
}

/// `value` as one shell word.
fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/@%+=,".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

/// A send's result. The human form prints the message alone on stdout, as for a new message,
/// and says on stderr when the message had already been sent.
fn print_message_receipt(receipt: &MessageSendReceipt, json_output: bool) -> Result<()> {
    if json_output {
        return print_value(receipt, true);
    }
    if receipt.already_sent {
        eprintln!(
            "st: {} was already sent{} (idempotency key {}); nothing new was sent",
            receipt.message.subject,
            receipt
                .sent_at
                .as_deref()
                .map(|sent_at| format!(" at {sent_at}"))
                .unwrap_or_default(),
            receipt.idempotency_key
        );
    }
    println!("{}", receipt.message.subject);
    Ok(())
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
) -> Result<ClaimRecord> {
    let reference = normalize_message_reference(reference);
    client
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
        .await
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

/// A complete `person/NAME` or an agent. Free mode lets an agent do what its person may do, and
/// the daemon still records the agent itself as the actor.
fn parse_actor_subject(actor: &str) -> std::result::Result<String, String> {
    if actor.starts_with("agent/") && actor.len() > "agent/".len() {
        return Ok(actor.to_owned());
    }
    parse_person_subject(actor)
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
        let identity = subject.strip_prefix("agent/").unwrap_or(subject);
        let paths = match st_drivers::driver_paths::Paths::from_environment(identity, &|name| {
            std::env::var(name).ok()
        })? {
            Some(paths) => NativePaths::from_resolved(subject, paths),
            None => NativePaths::legacy(subject, "claude")?,
        };
        if push_mailbox_enabled() {
            let incarnation = wait_for_agent_incarnation(client, subject).await?;
            return st3::claude_channel::run(
                client,
                subject,
                &incarnation,
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
        return run_st2_native_driver(client, subject, &args.driver, argv).await;
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
    let paths = NativePaths::prepare(subject, driver)?;
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
    let select = |argv: Vec<String>, session: &str| -> Result<_> {
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
    let argv = if let Some(session) = st3::native_resume::requested() {
        match select(argv, &session)? {
            Ok(argv) => argv,
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
        match select(argv.clone(), &session)? {
            Ok(argv) => argv,
            Err(refusal) => {
                skip_native_continue(client, subject, &incarnation, driver, &session, refusal)
                    .await;
                argv
            }
        }
    } else {
        argv
    };
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
    let task = spawn_st2_provider(driver, &paths, ProviderStart::Launch(argv));
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

/// Resolved observation and session paths for one native driver.
#[derive(Clone)]
struct NativePaths {
    delivery_gate: st_drivers::session_control::DeliveryGate,
    pending_hold_adoption: Option<st3::delivery_hold::HoldRequest>,
    driver_root: PathBuf,
    session_dir: PathBuf,
    agent_dir: PathBuf,
    identity: String,
    runtime_id: String,
}

impl NativePaths {
    fn prepare(subject: &str, driver: &str) -> Result<Self> {
        let (driver_root, agent_dir, identity, runtime_id) = prepare_native_driver(subject)?;
        let session_dir = driver_root.join("sessions").join(driver);
        fs::create_dir_all(&session_dir)?;
        Ok(Self {
            delivery_gate: st_drivers::session_control::DeliveryGate::default(),
            pending_hold_adoption: None,
            driver_root,
            session_dir,
            agent_dir,
            identity,
            runtime_id,
        })
    }

    fn resolved(&self) -> st_drivers::driver_paths::Paths {
        st_drivers::driver_paths::Paths {
            root: self.driver_root.clone(),
            agent_dir: self.agent_dir.clone(),
            session_dir: self.session_dir.clone(),
        }
    }

    fn from_resolved(subject: &str, paths: st_drivers::driver_paths::Paths) -> Self {
        let identity = subject.strip_prefix("agent/").unwrap_or(subject).to_owned();
        Self {
            delivery_gate: st_drivers::session_control::DeliveryGate::default(),
            pending_hold_adoption: None,
            driver_root: paths.root,
            agent_dir: paths.agent_dir,
            session_dir: paths.session_dir,
            runtime_id: identity.clone(),
            identity,
        }
    }

    /// A pre-change resume record adopts existing files without fabricating declarations.
    fn legacy(subject: &str, driver: &str) -> Result<Self> {
        let drivers = PathBuf::from(
            std::env::var_os("ST3_DRIVER_STATE_DIR")
                .context("the native driver has no ST3_DRIVER_STATE_DIR")?,
        );
        Self::legacy_in(subject, driver, &drivers)
    }

    fn legacy_in(subject: &str, driver: &str, drivers: &Path) -> Result<Self> {
        let driver_root = drivers
            .join(&hex::encode(Sha256::digest(subject.as_bytes()))[..24])
            .join("catalog");
        let identity = subject.strip_prefix("agent/").unwrap_or(subject).to_owned();
        let agent_dir =
            st3::hooks::legacy_claude_agent_dir(&drivers, subject, &st_drivers::run::detect_host());
        let session_dir = match driver {
            "claude" => st_drivers::claude_session::state_dir(&driver_root, &identity),
            "opencode" => st_drivers::opencode_session::state_dir(&driver_root, &identity),
            "codex" => driver_root.parent().unwrap().join("state"),
            _ => driver_root.parent().unwrap().join("sessions").join(driver),
        };
        Ok(Self::from_resolved(
            subject,
            st_drivers::driver_paths::Paths {
                root: driver_root,
                agent_dir,
                session_dir,
            },
        ))
    }

    fn resumed(
        subject: &str,
        driver: &str,
        paths: Option<st_drivers::driver_paths::Paths>,
    ) -> Result<Self> {
        match paths {
            Some(paths) => Ok(Self::from_resolved(subject, paths)),
            None => Self::legacy(subject, driver),
        }
    }

    /// Predecessor drivers wrote resume state beside their private catalog.
    fn state_root(&self) -> PathBuf {
        if self
            .driver_root
            .file_name()
            .is_some_and(|name| name == "catalog")
        {
            self.driver_root.parent().unwrap().to_path_buf()
        } else {
            self.driver_root.clone()
        }
    }
}

enum ProviderStart {
    Launch(Vec<String>),
    Adopt(st_drivers::provider_session::DetachedSession),
}

fn spawn_st2_provider(
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
    tokio::task::spawn_blocking(move || match start {
        ProviderStart::Launch(argv) => match driver.as_str() {
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
                    &paths.agent_dir,
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
    })
}

/// Driver loop state a replacement image carries forward, so it neither republishes what this
/// image already published nor forgets that the harness already reported ready.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct NativeLoopState {
    #[serde(default)]
    paths: Option<st_drivers::driver_paths::Paths>,
    ready: bool,
    /// Whether the harness-state file already belongs to this session. A resumed driver adopts a
    /// running session, so its record is always current.
    harness_record_started: bool,
    #[serde(skip)]
    predecessor_harness_record: Option<Vec<u8>>,
    published_timeline: BTreeSet<String>,
    delivery_episode: u64,
    #[serde(default)]
    mailbox_fence: Option<st3::mailbox::Fence>,
}

/// What a native driver hands its next image across `execve`.
#[derive(serde::Serialize, serde::Deserialize)]
struct DriverResume {
    driver: String,
    subject: String,
    incarnation: String,
    session: st_drivers::provider_session::DetachedSession,
    loop_state: NativeLoopState,
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
    let task = spawn_st2_provider(driver, &paths, ProviderStart::Adopt(resume.session));
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

/// Follows the installed st binary for a native driver. Once a replacement is ready, the driver
/// asks its provider task to detach and re-executes with the session it released.
struct DriverReplacement {
    watch: Option<st_drivers::reexec::ReplacementWatch>,
    pending: Option<PathBuf>,
}

impl DriverReplacement {
    fn new() -> Self {
        Self {
            watch: st_drivers::reexec::ReplacementWatch::for_current_process(),
            pending: None,
        }
    }

    /// Check the installed binary. Returns true when the provider task was asked to detach.
    fn check(&mut self) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let Some(watch) = self.watch.as_mut() else {
            return false;
        };
        let ready = tokio::task::block_in_place(|| watch.ready());
        let Some(binary) = ready else {
            return false;
        };
        self.pending = Some(binary);
        st_drivers::provider_session::DETACH.store(true, std::sync::atomic::Ordering::SeqCst);
        true
    }

    /// Re-execute with a released session. Returns only when that failed; the caller then adopts
    /// the session again in this image and retries the replacement later.
    fn exec(&mut self, subject: &str, state_root: &Path, resume: &DriverResume) -> anyhow::Error {
        st_drivers::provider_session::DETACH.store(false, std::sync::atomic::Ordering::SeqCst);
        let Some(binary) = self.pending.take() else {
            return anyhow::anyhow!("no replacement binary was pending");
        };
        let _ = write_driver_log(
            subject,
            &format!(
                "the st binary at {} was replaced; the {} driver re-executes into it and keeps its provider",
                binary.display(),
                resume.driver
            ),
        );
        let error = match st_drivers::reexec::write_state(state_root, "driver-resume", resume) {
            Ok(path) => {
                let error = st_drivers::reexec::exec_unless_stopped(
                    &binary,
                    st_drivers::reexec::DRIVER_RESUME_ENV,
                    &path,
                    &resume.session.inherited_descriptors(),
                    &st_drivers::provider_session::stop_requested,
                );
                let _ = fs::remove_file(&path);
                anyhow::Error::new(error).context(format!("executing {}", binary.display()))
            }
            Err(error) => error.context("saving the driver resume state"),
        };
        if let Some(watch) = self.watch.as_mut() {
            watch.refuse_current();
        }
        let _ = write_driver_log(
            subject,
            &format!(
                "the driver keeps its current binary and adopts its provider again: {error:#}"
            ),
        );
        error
    }
}

/// The detached session a provider task returned, if it returned one.
fn detached_session(outcome: &Result<()>) -> Option<st_drivers::provider_session::DetachedSession> {
    outcome
        .as_ref()
        .err()?
        .downcast_ref::<st_drivers::provider_session::Detached>()
        .map(|detached| detached.session.clone())
}

/// The delivery report a native driver attaches to its mailbox poll, so the daemon can tell a
/// current, live delivery path from a stale one.
fn native_delivery_report(transport: &str, agent_dir: Option<&Path>) -> String {
    let mut report = json!({
        "transport": transport,
        "pid": std::process::id(),
        "image": st_drivers::reexec::running_identity().map(|identity| identity.token()),
        "follows": st_drivers::reexec::installed_binary().map(|path| path.display().to_string()),
    });
    if let Some(agent_dir) = agent_dir {
        let channel = st_drivers::claude_mcp::read_presence(agent_dir);
        let now = current_unix_ms().unwrap_or_default() as u64;
        report["channel"] = match channel {
            Some(presence) => json!({
                "pid": presence.pid,
                "image": presence.image,
                "age_ms": now.saturating_sub(presence.at_unix_ms),
            }),
            None => Value::Null,
        };
    }
    report.to_string()
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
    let NativePaths {
        session_dir,
        agent_dir,
        identity,
        runtime_id,
        ..
    } = paths.clone();
    let mut mailbox =
        NativeMailbox::start(client, subject, &incarnation, driver, &mut loop_state).await?;
    let mut observations = NativeObservations::start(&agent_dir, &incarnation)?;
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
    let mut last_activity_fingerprint = None;
    let mut last_usage_fingerprint = None;
    let mut last_limits_fingerprint = None;
    let mut last_control_warning = None;
    let mut last_capacity_fingerprint = None;
    let mut delivery = NativeDeliverySupervisor::resumed(loop_state.delivery_episode);
    let mut replacement = DriverReplacement::new();
    let mut binding_watch = ClaudeBindingWatch::default();
    let mut reported_session = None;
    // Claude's hooks keep the subagent ledger; this driver records it on the seat.
    let mut subagents = (driver == "claude").then(|| {
        st3::subagents::Publisher::start(
            subject,
            driver,
            &incarnation,
            &agent_dir,
            st_drivers::subagents::now_ms(),
        )
    });
    loop {
        tokio::select! {
            frame = mailbox.recv() => {
                mailbox.accept(frame, &runtime_id)?;
            }
            wake = observations.recv() => {
                wake?;
                if let Err(error) = observations.drain(client, subject, driver, &mut loop_state.ready).await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
            }
            result = &mut task => {
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
                    task = spawn_st2_provider(driver, &paths, ProviderStart::Adopt(session));
                    continue;
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
                if let Err(error) = observations.expire_due() {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }

                if observations.retry_pending {
                    if let Err(error) = observations.drain(client, subject, driver, &mut loop_state.ready).await {
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                }

                if driver == "opencode" {
                    if let Err(error) = refresh_native_delivery_control(client, subject, &mut paths).await {
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
                if let Some(session) = bound
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
                if driver == "claude"
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
                    delivery.report = Some(native_delivery_report("opencode-server", None));
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
                let tick: Result<()> = async {
                    if observations.enabled { return Ok(()) }
                    if native_file_may_override_channel(driver)
                        && loop_state.harness_record_started
                        && let Some(observed) = st_drivers::harness_state::read(&harness_state_path, None)
                    {
                        // A session claim is a startup fence, not an observation. Preserve the
                        // explicit `starting` state until a hook or the initialized ST3 channel
                        // supplies positive evidence; publishing the derived `claimed`
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
                if let Err(error) = tick {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                if let Some(subagents) = subagents.as_mut()
                    && let Err(error) = subagents.tick(client).await
                {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                replacement.check();
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
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
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

/// How long a Claude wrapper session may run before a missing native-session binding is a fault.
/// SessionStart fires as Claude starts, so a minute covers a slow start.
const CLAUDE_BINDING_GRACE: Duration = Duration::from_secs(60);

/// Watches that the current Claude wrapper session gets its native-session binding. The seat's
/// SessionStart hook writes it; when the hook cannot run at all (its st3 binary is missing, or the
/// seat's settings name no hooks), only the driver can notice.
#[derive(Default)]
struct ClaudeBindingWatch {
    session: Option<String>,
    since: Option<Instant>,
    settled: bool,
}

impl ClaudeBindingWatch {
    /// The reason to record once `session` has run past the grace period with no binding. It is
    /// returned once per wrapper session.
    fn overdue(&mut self, agent_dir: &Path, session: Option<&str>, now: Instant) -> Option<String> {
        let session = session?;
        if self.session.as_deref() != Some(session) {
            self.session = Some(session.to_owned());
            self.since = Some(now);
            self.settled = false;
        }
        if self.settled {
            return None;
        }
        if st3::hooks::claude_binding(agent_dir, session).is_some() {
            self.settled = true;
            return None;
        }
        if now.duration_since(self.since?) < CLAUDE_BINDING_GRACE {
            return None;
        }
        self.settled = true;
        let variable = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unset".into())
        };
        Some(format!(
            "Claude has run for {} s and its SessionStart hook bound no native session, so st cannot find this seat's transcript. The hooks run \"$ST_HOOKS/claude-observe.sh\" (ST_HOOKS={}), which runs ST3_BIN={}.",
            CLAUDE_BINDING_GRACE.as_secs(),
            variable("ST_HOOKS"),
            variable("ST3_BIN"),
        ))
    }

    /// Report again on the next tick, after a report the daemon did not take.
    fn retry(&mut self) {
        self.settled = false;
    }
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
    let state_root = fs::canonicalize(state_root)
        .or_else(|_| {
            fs::create_dir_all(state_root)?;
            fs::canonicalize(state_root)
        })?
        .join(&hex::encode(Sha256::digest(subject.as_bytes()))[..24]);
    let agent_dir = state_root.join("observations");
    fs::create_dir_all(&agent_dir)?;
    let identity = subject.strip_prefix("agent/").unwrap_or(subject).to_owned();
    Ok((state_root, agent_dir, identity.clone(), identity))
}

fn harness_activity_state(activity: st_drivers::harness_state::Activity) -> &'static str {
    match activity {
        st_drivers::harness_state::Activity::Ready => "ready",
        st_drivers::harness_state::Activity::Idle => "idle",
        st_drivers::harness_state::Activity::Active | st_drivers::harness_state::Activity::Child => "working",
        st_drivers::harness_state::Activity::Ended => "ended",
        st_drivers::harness_state::Activity::Unknown => "indeterminate",
    }
}

/// A pipe wake follows a durable spool commit. The timer
/// retries a known pending publication only; it does not poll state/context/timeline records.
struct NativeObservations {
    dir: PathBuf,
    runtime: String,
    enabled: bool,
    provider_incarnation: Option<String>,
    evidence_deadline: Option<serde_json::Value>,
    retry_pending: bool,
    initial_wake: bool,
    pipe: Option<tokio::io::unix::AsyncFd<std::fs::File>>,
}
impl NativeObservations {
    fn start(dir: &Path, runtime: &str) -> Result<Self> {
        let enabled = st_drivers::harness_events::enabled(dir);
        let pipe = if enabled {
            Some(tokio::io::unix::AsyncFd::new(
                st_drivers::harness_events::bind_wake_pipe(dir)?,
            )?)
        } else {
            None
        };
        let provider_incarnation = if enabled {
            st_drivers::harness_state::read(
                &st_drivers::harness_state::harness_state_path(dir),
                None,
            )
            .and_then(|state| state.evidence_incarnation)
        } else {
            None
        };
        let evidence_deadline = if enabled {
            st_drivers::harness_events::read_snapshot(dir, "harness-state")?
                .and_then(|raw| serde_json::from_slice(&raw).ok())
        } else {
            None
        };
        Ok(Self {
            dir: dir.into(),
            runtime: runtime.into(),
            enabled,
            provider_incarnation,
            evidence_deadline,
            retry_pending: enabled,
            initial_wake: enabled,
            pipe,
        })
    }
    async fn recv(&mut self) -> Result<()> {
        if self.initial_wake {
            self.initial_wake = false;
            return Ok(());
        }
        let Some(pipe) = &self.pipe else {
            return std::future::pending().await;
        };
        loop {
            let mut ready = pipe.readable().await?;
            match ready.try_io(|descriptor| {
                use std::io::Read as _;
                let mut file = descriptor.get_ref();
                let mut bytes = [0; 256];
                file.read(&mut bytes)
            }) {
                Ok(Ok(count)) if count > 0 => return Ok(()),
                Ok(Ok(_)) => anyhow::bail!("event wake pipe closed"),
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => continue,
            }
        }
    }
    fn expire_due(&mut self) -> Result<()> {
        if let Some(state) = &self.evidence_deadline {
            let stamp = state["writtenAtMs"]
                .as_u64()
                .context("state evidence has no timestamp")?;
            let stale = st_drivers::harness_state::HARNESS_STATE_STALE.as_millis() as u64;
            if current_unix_ms()? >= u128::from(stamp.saturating_add(stale)) {
                st_drivers::harness_events::expire_state(&self.dir, state)?;
                self.evidence_deadline = None;
                self.retry_pending = true;
            }
        }
        Ok(())
    }
    async fn drain(
        &mut self,
        client: &Client,
        subject: &str,
        driver: &str,
        ready: &mut bool,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        self.retry_pending = true;
        // Bound a wake's work so a backlog does not hold back native delivery.
        let events = st_drivers::harness_events::pending(&self.dir, 64)?;
        for event in &events {
            let publisher = ObservationClient {
                client,
                event: Some((&self.runtime, event.sequence, &self.dir, event.payload["account_ref"].as_str())),
            };
            let raw = serde_json::to_vec(&event.payload)?;
            let source_driver = event.payload["harness"]
                .as_str()
                .or_else(|| event.payload["driver"].as_str())
                .context("event has no source driver")?;
            anyhow::ensure!(
                matches!(
                    source_driver,
                    "claude" | "codex" | "pi" | "omp" | "opencode"
                ),
                "unknown event driver"
            );
            match event.kind.as_str() {
                "harness-state" | "harness-state-expired" => {
                    let measured = event.payload["writtenAtMs"]
                        .as_u64()
                        .context("state event has no timestamp")?;
                    let decode_at = if event.kind == "harness-state-expired" {
                        measured.saturating_add(
                            st_drivers::harness_state::HARNESS_STATE_STALE.as_millis() as u64,
                        )
                    } else {
                        event.queued_at_ms
                    };
                    let observed = st_drivers::harness_state::read_raw_at(&raw, None, decode_at);
                    if event.runtime_incarnation == self.runtime && source_driver == driver {
                        self.provider_incarnation = observed.evidence_incarnation.clone();
                        self.evidence_deadline =
                            (event.kind != "harness-state-expired").then(|| event.payload.clone());
                    }
                    let placeholder = observed.state
                        == st_drivers::harness_state::Activity::Unknown
                        && observed.reason.as_deref() == Some("claimed");
                    if !placeholder {
                        publish_harness_activity(
                            &publisher,
                            subject,
                            source_driver,
                            match source_driver {
                                "claude" => "claude-channel",
                                "codex" => "app-server",
                                "pi" => "pi-channel",
                                "omp" => "omp-channel",
                                _ => "native",
                            },
                            Some(&event.runtime_incarnation),
                            &observed,
                            &mut None,
                        )
                        .await?;
                        if driver != "codex"
                            && event.runtime_incarnation == self.runtime
                            && source_driver == driver
                            && !matches!(
                                observed.state,
                                st_drivers::harness_state::Activity::Unknown
                                    | st_drivers::harness_state::Activity::Ended
                            )
                        {
                            *ready = true;
                        }
                        if observed.reason.as_deref() == Some("providerCapacity") {
                            let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&(
                                source_driver,
                                observed.since_ms,
                                observed.reason.as_deref(),
                            ))?));
                            publish_provider_capacity_diagnostic(
                                client,
                                subject,
                                &event.runtime_incarnation,
                                observed.since_ms,
                                &fingerprint,
                            )
                            .await?;
                        }
                    }
                }
                "harness-context" => {
                    let observed =
                        st_drivers::harness_context::read_raw_at(&raw, event.queued_at_ms)
                            .context("invalid context event")?;
                    publish_harness_usage(
                        &publisher,
                        subject,
                        source_driver,
                        &event.runtime_incarnation,
                        &observed,
                        &mut None,
                    )
                    .await?;
                    publish_harness_limits(
                        &publisher,
                        subject,
                        source_driver,
                        &event.runtime_incarnation,
                        &observed,
                        &mut None,
                    )
                    .await?;
                }
                "harness-todo" => {
                    let mut fields: BTreeMap<String, Value> =
                        serde_json::from_value(event.payload.clone())?;
                    fields.remove("incarnation");
                    fields.insert("incarnation_id".into(), event.runtime_incarnation.clone().into());
                    let _: ClaimRecord = publisher.post(
                        "/v1/claims",
                        &ClaimInput {
                            subject: subject.into(),
                            kind: "harness.todo.observed".into(),
                            actor: Some(subject.into()),
                            fields,
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: Some(format!(
                                "harness-todo:{subject}:{}:{}", event.runtime_incarnation, event.sequence,
                            )),
                        },
                    ).await?;
                }
                "harness-timeline" => {
                    let operation: st_drivers::harness_timeline::Operation =
                        serde_json::from_slice(&raw)?;

                    let fields = timeline_claim_fields(operation, &event.runtime_incarnation);
                    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&fields)?));
                    let _: ClaimRecord = publisher
                        .post(
                            "/v1/claims",
                            &ClaimInput {
                                subject: subject.into(),
                                kind: "harness.timeline".into(),
                                actor: Some(subject.into()),
                                fields,
                                evidence: Vec::new(),
                                expected_subject: None,
                                idempotency_key: Some(format!(
                                    "harness-timeline:{subject}:{digest}"
                                )),
                            },
                        )
                        .await?;
                }
                other => anyhow::bail!("unsupported harness event `{other}`"),
            }
            st_drivers::harness_events::acknowledge(&self.dir, event.sequence)?;
        }
        self.retry_pending = events.len() == 64;
        self.initial_wake = self.retry_pending;
        Ok(())
    }
}

struct ObservationClient<'a> {
    client: &'a Client,
    event: Option<(&'a str, u64, &'a Path, Option<&'a str>)>,
}
impl std::ops::Deref for ObservationClient<'_> {
    type Target = Client;
    fn deref(&self) -> &Client {
        self.client
    }
}
impl ObservationClient<'_> {
    async fn post<O: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        claim: &ClaimInput,
    ) -> Result<O> {
        let account = match self.event {
            Some((_, _, _, account)) => account.map(str::to_owned),
            None => std::env::var("ST3_ACCOUNT").ok(),
        };
        let mut claim = claim.clone();
        bind_observation_account(&mut claim, account.as_deref());
        if let Some((runtime, sequence, dir, _)) = self.event {
            let slot = format!(
                "{}:{}",
                claim.kind,
                claim
                    .fields
                    .get("semantics")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            );
            let claim = serde_json::from_value(st_drivers::harness_events::prepare_publication(
                dir,
                sequence,
                &slot,
                &serde_json::to_value(&claim)?,
            )?)?;
            self.client
                .post(
                    "/v1/harness-events",
                    &st3::harness_events::Publication {
                        runtime_incarnation: runtime.into(),
                        sequence,
                        claim,
                    },
                )
                .await
        } else {
            self.client.post(path, &claim).await
        }
    }
}

/// Bound accounts have their own accounting identity even when a provider reports only a
/// generic API-key label or no identity. Unbound seats retain the provider's existing identity.
fn bind_observation_account(claim: &mut ClaimInput, account: Option<&str>) {
    if claim.kind == "harness.limits" {
        claim.fields.remove("account_ref");
    }
    let Some(account) = account.filter(|name| !name.is_empty()) else {
        return;
    };
    let Some(driver) = claim.fields.get("driver").and_then(Value::as_str) else {
        return;
    };
    let label = st_drivers::account::account_label(driver, &format!("declared:{account}"));
    match claim.kind.as_str() {
        "harness.limits" => {
            claim
                .fields
                .insert("account_ref".into(), Value::String(account.into()));
            claim.fields.insert("account".into(), Value::String(label));
        }
        "harness.usage" => {
            claim.fields.insert("account".into(), Value::String(label));
        }
        "harness.timeline"
            if claim.fields.get("entry_type").and_then(Value::as_str) == Some("usage") =>
        {
            if let Some(Value::Object(body)) = claim.fields.get_mut("body") {
                body.insert("account".into(), Value::String(label));
            }
        }
        _ => {}
    }
}

async fn publish_harness_activity(
    client: &ObservationClient<'_>,
    subject: &str,
    driver: &str,
    transport: &str,
    incarnation: Option<&str>,
    observed: &st_drivers::harness_state::Observed,
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
    st3::suspension::annotate_quiescence(&mut fields);
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

/// Publish the harness's latest reading of its paying account's limits when any of it changed.
/// Readers take the freshest reading per account across the fleet, so the reading keeps the time
/// the harness measured it, not when st published it.
async fn publish_harness_limits(
    client: &ObservationClient<'_>,
    subject: &str,
    driver: &str,
    incarnation: &str,
    observed: &st_drivers::harness_context::Observed,
    last_fingerprint: &mut Option<String>,
) -> Result<()> {
    let limits = observed.rate_limits;
    if limits.five_hour.is_none() && limits.seven_day.is_none() {
        return Ok(());
    }
    let mut fields = BTreeMap::from([
        ("driver".into(), Value::String(driver.into())),
        ("incarnation_id".into(), Value::String(incarnation.into())),
        (
            "measured_at_unix_ms".into(),
            Value::from(observed.observed_at_ms),
        ),
    ]);
    for (name, value) in [("account", &observed.account), ("plan", &observed.plan)] {
        if let Some(value) = value {
            fields.insert(name.into(), Value::String(value.clone()));
        }
    }
    for (name, value) in [
        ("five_hour_percent", limits.five_hour),
        ("weekly_percent", limits.seven_day),
    ] {
        if let Some(value) = value.filter(|value| value.is_finite()) {
            fields.insert(name.into(), Value::from(value));
        }
    }
    for (name, value) in [
        ("five_hour_resets_at_unix_ms", limits.five_hour_resets_at_ms),
        ("weekly_resets_at_unix_ms", limits.seven_day_resets_at_ms),
    ] {
        if let Some(value) = value {
            fields.insert(name.into(), Value::from(value));
        }
    }
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&fields)?));
    if last_fingerprint.as_deref() == Some(fingerprint.as_str()) {
        return Ok(());
    }
    let _: ClaimRecord = client
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: subject.into(),
                kind: "harness.limits".into(),
                actor: Some(subject.into()),
                fields,
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!("harness-limits:{subject}:{fingerprint}")),
            },
        )
        .await?;
    *last_fingerprint = Some(fingerprint);
    Ok(())
}

async fn publish_harness_usage(
    client: &ObservationClient<'_>,
    subject: &str,
    driver: &str,
    incarnation: &str,
    observed: &st_drivers::harness_context::Observed,
    last_fingerprint: &mut Option<String>,
) -> Result<()> {
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
        let fields = timeline_claim_fields(operation, incarnation);
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&fields)?));
        let _: ClaimRecord = ObservationClient { client, event: None }
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
    record: &st_drivers::harness_timeline::Record,
    driver: &str,
    provider_incarnation: Option<&str>,
) -> bool {
    record.driver == driver && provider_incarnation == Some(record.incarnation_id.as_str())
}

fn timeline_claim_fields(
    operation: st_drivers::harness_timeline::Operation,
    runtime_incarnation: &str,
) -> BTreeMap<String, Value> {
    let mut fields = BTreeMap::from([
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
    ]);
    if fields["entry_type"] == "usage"
        && let Some(source_id) = operation.source_id
    {
        fields.insert("source_id".into(), Value::String(source_id));
    }
    fields
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
    let diagnostic = ClaimInput {
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
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(st3::suspension::continue_unavailable_key(subject, session)),
    };
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

/// A `harness.observed` claim's fields with the driver's quiescence report added.
fn with_quiescence(mut fields: BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    st3::suspension::annotate_quiescence(&mut fields);
    fields
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

/// One pi-family message frame. The content is the shared `<smalltalk-message>` envelope that
/// Codex also receives, steered into a running turn at its next tool boundary. omp backgrounds an in-flight
/// shell or eval call when a steer arrives, and one live omp seat then repeated a send whose
/// result it had not seen. Queueing mail with `followUp` instead was measured and was worse: omp
/// read the queued messages during its turn, the queue then re-delivered them as new prompts, and
/// seats that answered those stale prompts declined the next real task in three of six
/// cross-harness runs.
fn pi_family_message_frame(
    message: &st3::model::MessageView,
    body: &str,
    identity: &str,
    attachments: &[st_drivers::ding::AttachmentNotice],
) -> Value {
    json!({
        "type": "message",
        "deliverAs": "steer",
        "content": st_drivers::ding::with_dictation_notice(st_drivers::ding::st3_notification_with_attachments(
            &message.subject,
            &message.from,
            &message.to,
            message.title.as_deref(),
            body,
            &st_drivers::ding::st3_body_sha256(body),
            attachments,
        ), &message.tags),
        "meta": {
            "from": message.from,
            "messageId": message.subject,
            "threadId": message.in_reply_to.clone().unwrap_or_else(|| message.subject.clone()),
            "identity": identity,
        },
    })
}

fn accept_managed_channel_frame(
    state: &mut PiChannelResume,
    observer: &mut Option<st_drivers::pi_channel::EventObserver>,
    line: &str,
) -> Result<bool> {
    if let Some(observer) = observer.as_mut()
        && let Ok(frame) = serde_json::from_str::<Value>(line)
    {
        observer.observe(&frame)?;
        if frame["type"] == "todo" {
            // The managed observer committed this snapshot to the durable outbox already.
            return Ok(false);
        }
    }
    let publish = state.accept_frame(line);
    if observer.is_some() {
        state.pending.state = None;
    }
    Ok(publish)
}

/// Session-start context for a pi-family seat: only the seat's saved context, which the extension
/// adds without starting a turn. st adds no instructions of its own, so a new seat stays idle until
/// a person types or a message is posted.
fn pi_family_session_context(identity: &str, context: &str) -> String {
    if context.trim().is_empty() {
        return String::new();
    }
    format!(
        "<context source=\"st3/context/now.md\" agent=\"{identity}\">\n{}\n</context>",
        context.trim_end()
    )
}

/// One seat's spool retains its sequence across channel reconnects, but not ended incarnations.
fn prepare_channel_todo_outbox(root: &Path, subject: &str, incarnation: &str) -> Result<PathBuf> {
    let seat = hex::encode(Sha256::digest(subject.as_bytes()));
    let token = hex::encode(Sha256::digest(incarnation.as_bytes()));
    let seat_dir = root.join(".st3-channel-outbox").join(seat);
    fs::create_dir_all(&seat_dir)?;
    for entry in fs::read_dir(&seat_dir)? {
        let entry = entry?;
        if entry.file_name() != token.as_str() && entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(seat_dir.join(token))
}

fn todo_runtime_has_ended(actual: &Value, incarnation: &str) -> bool {
    let fields = actual.get("fields").unwrap_or(actual);
    fields["incarnation_id"].as_str().is_some_and(|current| current != incarnation)
        || matches!(fields["status"].as_str(), Some("absent" | "stopped" | "exited" | "vanished"))
}

async fn remove_ended_channel_todo_outbox(
    client: &Client, subject: &str, incarnation: &str, dir: &Path,
) -> Result<bool> {
    let status: StatusResponse = client.get(&format!(
        "/v1/status?subject={}", urlencoding::encode(subject),
    )).await?;
    if status.subjects.first().and_then(|seat| seat.actual.as_ref())
        .is_some_and(|actual| todo_runtime_has_ended(actual, incarnation))
    {
        fs::remove_dir_all(dir)?;
        return Ok(true);
    }
    Ok(false)
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
    let mut stdout = tokio::io::stdout();
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
    let mut todo_observations = if driver == "omp" && observer.is_none() {
        anyhow::ensure!(
            retry_while_daemon_unreachable(subject, || current_agent_incarnation(client, subject))
                .await?.as_deref() == Some(&incarnation),
            "todo channel runtime was superseded",
        );
        let dir = prepare_channel_todo_outbox(catalog, subject, &incarnation)?;
        st_drivers::harness_events::enable(&dir, &incarnation)?;
        state.todo_outbox = Some(dir.clone());
        Some(NativeObservations::start(&dir, &incarnation)?)
    } else {
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
    let mut checked_todo_minute = None;
    loop {
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
                            match accept_managed_channel_frame(&mut state, &mut observer, &line) {
                                Ok(changed) => publish |= changed,
                                Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
                            }
                        }
                    }
                    // The extension ends the pipe when its session ends.
                    Some(st_drivers::reexec::StdinChunk::Eof) | None => {
                        if let Some(line) = state.lines.finish()
                            && accept_managed_channel_frame(&mut state, &mut observer, &line)?
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
                            if let Err(error) = remove_ended_channel_todo_outbox(
                                client, subject, &incarnation, &observations.dir,
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
                    for message in page.items.into_iter().filter(|message| matches!(message.status.as_str(), "sent" | "staged" | "delivered")) {
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
                        publish |= accept_managed_channel_frame(&mut state, &mut observer, &line)?;
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
            _ = work_interval.tick() => {
                let minute = unix_minute()?;
                if checked_todo_minute != Some(minute) {
                    checked_todo_minute = Some(minute);
                    if let Some(observations) = todo_observations.as_ref() {
                        match remove_ended_channel_todo_outbox(
                            client, subject, &incarnation, &observations.dir,
                        ).await {
                            Ok(true) => return Ok(()),
                            Ok(false) => {}
                            Err(error) => warn_pi_channel(subject, &error, &mut last_warning),
                        }
                    }
                }
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

/// Everything a pi-family channel keeps between frames, which is also what it hands its next
/// image when the st binary is replaced.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PiChannelResume {
    incarnation: String,
    session: String,
    #[serde(default)]
    native_session: Option<String>,
    #[serde(default)]
    todo_outbox: Option<PathBuf>,
    delivered: BTreeSet<String>,
    failed_handoffs: BTreeMap<String, u32>,
    #[serde(default)]
    retry_after_ms: BTreeMap<String, u64>,
    failed_diagnostics: BTreeSet<String>,
    first_idle_seen: bool,
    frame_sequence: u64,
    // Reports the daemon has not accepted yet. A restart must not end the channel or lose the
    // harness's latest state, so each waits here and is sent again on the next tick.
    pending: PiFamilyReports,
    lines: st_drivers::reexec::LineBuffer,
}

impl PiChannelResume {
    /// Apply one frame from the extension. Returns whether it left a report to publish.
    fn accept_frame(&mut self, line: &str) -> bool {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("todo") => {
                let Some(fields) = st_drivers::pi_channel::todo_observation(
                    &frame, "omp", self.native_session.as_deref(), &self.incarnation,
                ) else {
                    return false;
                };
                if let Some(dir) = &self.todo_outbox
                    && let Err(error) = st_drivers::harness_events::write_channel_todo(
                        dir, &self.incarnation, &json!(fields),
                    )
                {
                    tracing::warn!("st omp channel: recording todo failed: {error:#}");
                }
                true
            }
            Some("state") => {
                let Some(state) = frame.get("state").and_then(Value::as_str) else {
                    return false;
                };
                let status = match state {
                    "active" => "working",
                    "idle" => {
                        self.first_idle_seen = true;
                        "idle"
                    }
                    _ => return false,
                };
                self.frame_sequence = self.frame_sequence.saturating_add(1);
                self.pending.state = Some((status.to_owned(), self.frame_sequence));
                self.pending.blocked_on = frame
                    .get("blockedOn")
                    .and_then(Value::as_str)
                    .filter(|word| *word == "human")
                    .map(str::to_owned);
                self.pending.ask = if self.pending.blocked_on.is_some() {
                    frame.get("ask").and_then(Value::as_str).map(str::to_owned)
                } else {
                    None
                };
                self.pending.reason = frame
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|reason| reason.chars().take(2_000).collect());
                true
            }
            // The extension names the native session it runs, before and after its hello.
            Some("session" | "ready") => {
                let Some(native) = frame
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    return false;
                };
                let path = frame
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.pending.native_session = Some((native.to_owned(), path));
                self.native_session = Some(native.to_owned());
                true
            }
            Some("delivered") => {
                let Some(message) = frame.pointer("/meta/messageId").and_then(Value::as_str) else {
                    return false;
                };
                self.failed_handoffs.remove(message);
                self.retry_after_ms.remove(message);
                self.failed_diagnostics.remove(message);
                self.pending.acknowledgements.insert(message.to_owned());
                true
            }
            Some("read") => {
                let Some(message) = frame.pointer("/meta/messageId").and_then(Value::as_str) else {
                    return false;
                };
                self.pending.acknowledgements.insert(message.into());
                self.pending.reads.insert(message.into());
                true
            }
            Some("failed") => {
                let Some(message) = frame.pointer("/meta/messageId").and_then(Value::as_str) else {
                    return false;
                };
                let failures = self.failed_handoffs.entry(message.to_owned()).or_default();
                *failures = failures.saturating_add(1);
                // A negative native receipt authorizes another attempt. Keep retrying after
                // the diagnostic threshold; a temporary failure must never strand the head.
                let delay_ms = (500u64 << (*failures).min(4)).min(5_000);
                self.retry_after_ms.insert(
                    message.to_owned(),
                    (current_unix_ms().unwrap_or_default() as u64).saturating_add(delay_ms),
                );
                self.delivered.remove(message);
                if *failures >= 3 {
                    self.failed_diagnostics.insert(message.to_owned());
                }
                false
            }
            _ => false,
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
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PiFamilyReports {
    /// Only the latest state matters; a newer frame replaces an unsent older one.
    state: Option<(String, u64)>,
    // Keep the state tuple's resume wire shape: an older image can leave a pending state.
    // Missing axes in that image mean unblocked, never a sparse update to an older ask.
    #[serde(default)]
    blocked_on: Option<String>,
    #[serde(default)]
    ask: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    acknowledgements: BTreeSet<String>,
    #[serde(default)]
    reads: BTreeSet<String>,
    #[serde(default)]
    fence: Option<st3::mailbox::Fence>,
    /// The native session the harness reported, with its transcript, until the daemon has it.
    #[serde(default)]
    native_session: Option<(String, Option<String>)>,
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
        if let Some((status, sequence)) = self.state.clone() {
            let _: ClaimRecord = client
                .post(
                    "/v1/claims",
                    &ClaimInput {
                        subject: subject.into(),
                        kind: "harness.observed".into(),
                        actor: Some(subject.into()),
                        fields: with_quiescence(BTreeMap::from([
                            ("state".into(), Value::String(status)),
                            ("driver".into(), Value::String(driver.into())),
                            (
                                "transport".into(),
                                Value::String(format!("{driver}-channel")),
                            ),
                            ("incarnation_id".into(), Value::String(incarnation.into())),
                            (
                                "blocked_on".into(),
                                self.blocked_on
                                    .clone()
                                    .map(Value::String)
                                    .unwrap_or(Value::Null),
                            ),
                            (
                                "ask".into(),
                                self.ask.clone().map(Value::String).unwrap_or(Value::Null),
                            ),
                            (
                                "reason".into(),
                                self.reason
                                    .clone()
                                    .map(Value::String)
                                    .unwrap_or(Value::Null),
                            ),
                            ("input_buffer".into(), Value::Null),
                            ("exit".into(), Value::Null),
                        ])),
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
            match &self.fence {
                Some(fence) => mailbox_receipt(client, fence, &message, "delivered").await?,
                None => acknowledge_pi_family_delivery(client, subject, &message).await?,
            }
            self.acknowledgements.remove(&message);
        }
        while let Some(message) = self.reads.first().cloned() {
            if let Some(fence) = &self.fence {
                mailbox_receipt(client, fence, &message, "read").await?;
                use tokio::io::AsyncWriteExt as _;
                let mut stdout = tokio::io::stdout();
                stdout.write_all(format!("{}\n", json!({"type":"settled","meta":{"messageId":message}})).as_bytes()).await?;
                stdout.flush().await?;
            }
            self.reads.remove(&message);
        }
        // Last, and never fatal: a report the daemon does not take must not hold back delivery.
        // It stays pending and goes again with the next report. A session is reported only once
        // its transcript exists, because only then can a resume find it: pi writes nothing until
        // its first turn.
        let resumable = |native: &str| {
            std::env::var_os(st_drivers::driver_paths::SESSION_DIR_ENV).is_none_or(|dir| {
                st3::native_resume::pi_family_transcript(
                    &PathBuf::from(dir).join("provider-sessions"),
                    native,
                )
                .is_some()
            })
        };
        if let Some((native, path)) = self.native_session.clone()
            && resumable(&native)
        {
            let reported: Result<ClaimRecord> = client
                .post(
                    "/v1/agents/native-session",
                    &json!({
                        "subject": subject,
                        "actor": subject,
                        "incarnation_id": incarnation,
                        "harness": driver,
                        "session_id": native,
                        "path": path,
                        "account_ref": std::env::var("ST3_ACCOUNT").ok(),
                    }),
                )
                .await;
            match reported {
                Ok(_) => self.native_session = None,
                Err(error) => {
                    let _ = write_driver_log(
                        subject,
                        &json!({"type":"native_session_report_failed","error":format!("{error:#}")})
                            .to_string(),
                    );
                }
            }
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
        Ok(claim) => Ok(claim.kind == "message.staged"),
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

async fn refresh_graph_delivery_gate(
    client: &Client,
    subject: &str,
    gate: &st_drivers::session_control::DeliveryGate,
) -> Result<()> {
    let path = format!("/v1/delivery/hold?subject={}", urlencoding::encode(subject));
    let read = client.get::<st3::delivery_hold::HoldView>(&path);
    let result: Result<_> = async {
        let view = tokio::time::timeout(Duration::from_millis(250), read)
            .await
            .context("delivery hold read timed out")??;
        anyhow::ensure!(
            view.subject == subject,
            "delivery control names a different seat"
        );
        Ok(view.active)
    }
    .await;
    gate.update(
        result.as_ref().copied().unwrap_or(true),
        Duration::from_secs(3),
    );
    result
        .map(|_| ())
        .context("new native handoffs held until graph delivery control is available")
}

async fn refresh_native_delivery_control(
    client: &Client,
    subject: &str,
    paths: &mut NativePaths,
) -> Result<()> {
    let adoption: Result<()> = async {
        if let Some(request) = &paths.pending_hold_adoption {
            // The previous hold can expire during an outage. Never import it with a new deadline.
            if current_unix_ms()? < u128::from(request.until_unix_ms) {
                tokio::time::timeout(
                    Duration::from_millis(250),
                    client.post::<_, ClaimRecord>("/v1/delivery/hold", request),
                )
                .await
                .context("delivery hold adoption timed out")??;
            }
            paths.pending_hold_adoption = None;
        }
        Ok(())
    }
    .await;
    if let Err(error) = adoption {
        paths.delivery_gate.update(true, Duration::ZERO);
        return Err(error
            .context("native delivery held while the predecessor's hold awaits graph adoption"));
    }
    refresh_graph_delivery_gate(client, subject, &paths.delivery_gate).await
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
        ProviderStart::Launch(Vec::new()),
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

fn spawn_codex_provider(
    paths: &NativePaths,
    state_dir: &Path,
    argv: &[String],
    start: ProviderStart,
) -> tokio::task::JoinHandle<Result<()>> {
    let paths = paths.clone();
    let state_dir = state_dir.to_path_buf();
    let argv = argv.to_vec();
    tokio::task::spawn_blocking(move || match start {
        // A resumed seat's launch environment names the thread it suspended on, and any other
        // relaunch the thread it continues.
        ProviderStart::Launch(_) => {
            let thread = st3::native_resume::requested().or_else(|| codex_continued_thread(&argv));
            st_drivers::codex_app_server::run_controlled_paths(
                &paths.driver_root,
                &state_dir,
                &paths.agent_dir,
                paths.identity,
                paths.runtime_id,
                argv,
                paths.delivery_gate,
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
        ),
        ProviderStart::Adopt(session) => {
            anyhow::bail!("a Codex driver cannot adopt this provider session: {session:?}")
        }
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
    let mut paths = if matches!(start, ProviderStart::Launch(_)) {
        NativePaths::prepare(subject, "codex")?
    } else {
        NativePaths::resumed(subject, "codex", loop_state.paths.clone())?
    };
    if matches!(start, ProviderStart::Launch(_)) {
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
    if matches!(start, ProviderStart::Launch(_)) {
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
    let mut observations = NativeObservations::start(&agent_dir, &incarnation)?;
    if push_mailbox_enabled() {
        st_drivers::push_mailbox::register(&agent_dir);
    }
    if matches!(start, ProviderStart::Adopt(_)) {
        paths.pending_hold_adoption = legacy_delivery_hold(subject, &paths.agent_dir);
    }
    // The thread this launch continues; a refusal of it ends the wrapper before it binds.
    let continued = matches!(start, ProviderStart::Launch(_))
        .then(|| codex_continued_thread(&argv))
        .flatten();
    let mut task = spawn_codex_provider(&paths, &state_dir, &argv, start);
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
    let mut last_activity_fingerprint = None;
    let mut last_usage_fingerprint = None;
    let mut last_limits_fingerprint = None;
    let mut last_control_warning = None;
    let mut last_capacity_fingerprint = None;
    let mut delivery = NativeDeliverySupervisor::resumed(loop_state.delivery_episode);
    let mut replacement = DriverReplacement::new();
    loop {
        tokio::select! {
            frame = mailbox.recv() => { mailbox.accept(frame, &runtime_id)?; }

            wake = observations.recv() => {
                wake?;
                if let Err(error) = observations.drain(client, subject, "codex", &mut loop_state.ready).await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
            }
            result = &mut task => {
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
                    task = spawn_codex_provider(&paths, &state_dir, &argv, ProviderStart::Adopt(session));
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
            _ = interval.tick() => {
                if let Err(error) = observations.expire_due() {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }

                if observations.retry_pending {
                    if let Err(error) = observations.drain(client, subject, "codex", &mut loop_state.ready).await {
                        note_driver_tick_failure(subject, error, &mut last_control_warning);
                    }
                }

                if let Err(error) = refresh_native_delivery_control(client, subject, &mut paths).await {
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
                delivery.report = Some(native_delivery_report("app-server", None));
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
                }
                let tick: Result<()> = async {
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
                if let Err(error) = tick {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                subagents.set_timeline_incarnation(
                    st_drivers::codex_app_server::current_runtime_incarnation(
                        &state_dir,
                        &identity,
                        &runtime_id,
                    ),
                );
                if let Err(error) = subagents.tick(client).await {
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
                }
                // Only a bound session can be adopted, so wait for the binding before following
                // a replacement.
                if loop_state.ready {
                    replacement.check();
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
                    note_driver_tick_failure(subject, error, &mut last_control_warning);
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
    let mut failure = None;
    for step in work
        .into_iter()
        .filter(|step| work_claim_has_active_harness(step, subject, harness))
    {
        let renewed: Result<StepRunView> = client
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
            .await;
        match renewed {
            Ok(_) => {}
            // The claim moved on between reading the work and renewing it. That step no longer
            // needs this lease, and the driver's other steps still do.
            Err(error) if renewal_lost_its_claim(&error) => {
                let _ = write_driver_log(
                    subject,
                    &format!("skip renewing {}: {error:#}", step.subject),
                );
            }
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

/// Ask the daemon to suspend or resume `subject`, then wait for the request to finish: the seat
/// suspended, or resumed with its harness bound to the suspended native session.
async fn request_suspension(
    endpoint: &Endpoint,
    route: &str,
    subject: &str,
    actor: &str,
    reason: Option<&str>,
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

/// Whether a renewal failed only because the step's claim ended or moved to another incarnation.
fn renewal_lost_its_claim(error: &anyhow::Error) -> bool {
    matches!(
        st3::client::api_error_code(error),
        Some("work-not-claimed" | "wrong-work-incarnation")
    )
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
    /// The delivery report attached to each mailbox poll; see [`native_delivery_report`].
    report: Option<String>,
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
            report: None,
            daemon_outage: false,
            failures: 0,
            retry_after: None,
            degraded_recorded: false,
            last_warning: None,
        }
    }
}

impl NativeDeliverySupervisor {
    /// A supervisor that continues a predecessor image's episode numbering, so a new failure
    /// episode never reuses the idempotency key of one already recorded.
    fn resumed(episode: u64) -> Self {
        Self {
            episode,
            ..Self::default()
        }
    }

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

async fn report_unforwarded_message(
    client: &Client,
    subject: &str,
    incarnation: &str,
    transport: &str,
    message: &str,
    reason: &str,
) -> Result<()> {
    let _: ClaimRecord = client
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: subject.into(),
                kind: "harness.diagnostic".into(),
                actor: Some(subject.into()),
                fields: BTreeMap::from([
                    ("severity".into(), Value::String("warning".into())),
                    ("status".into(), Value::String("degraded".into())),
                    ("code".into(), Value::String("message-unforwarded".into())),
                    (
                        "reason".into(),
                        Value::String(format!("{message} could not be forwarded: {reason}")),
                    ),
                    ("incarnation_id".into(), Value::String(incarnation.into())),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some(format!(
                    "message-unforwarded:{subject}:{incarnation}:{transport}:{message}"
                )),
            },
        )
        .await?;
    Ok(())
}

fn push_mailbox_enabled() -> bool {
    std::env::var("ST3_MAILBOX_TRANSPORT").as_deref() == Ok("push")
}

struct NativeMailbox {
    subscription: Option<st3::mailbox::Subscription>,
    fence: st3::mailbox::Fence,
    messages: Vec<MessageView>,
    queued: BTreeMap<String, st_drivers::message::Message>,
    replayed: bool,
}
impl NativeMailbox {
    async fn start(
        client: &Client,
        subject: &str,
        incarnation: &str,
        driver: &str,
        state: &mut NativeLoopState,
    ) -> Result<Self> {
        let component = if matches!(driver, "codex" | "opencode") {
            "delivery"
        } else {
            "title"
        };
        let mut fence = state
            .mailbox_fence
            .get_or_insert_with(|| st3::mailbox::Fence::new(subject, incarnation, component))
            .clone();
        if push_mailbox_enabled() {
            fence.bind(client).await?;
            state.mailbox_fence = Some(fence.clone());
        }
        let transport = if driver == "codex" {
            "app-server"
        } else {
            "opencode-server"
        };
        let report: Value =
            serde_json::from_str(&native_delivery_report(transport, None)).unwrap_or_default();
        let subscription = push_mailbox_enabled()
            .then(|| st3::mailbox::Subscription::start(client.clone(), fence.clone(), report));
        Ok(Self {
            subscription,
            fence,
            messages: Vec::new(),
            queued: BTreeMap::new(),
            replayed: false,
        })
    }
    async fn recv(&mut self) -> Option<st3::mailbox::Frame> {
        match &mut self.subscription {
            Some(subscription) => subscription.receiver.recv().await,
            None => std::future::pending().await,
        }
    }
    fn accept(&mut self, frame: Option<st3::mailbox::Frame>, runtime_id: &str) -> Result<()> {
        match frame {
            Some(st3::mailbox::Frame::Seat { seat }) => {
                if let Err(error) = update_native_title(&seat, runtime_id) {
                    eprintln!("st: could not update seat title: {error:#}");
                }
                Ok(())
            }
            Some(st3::mailbox::Frame::Mailbox { messages }) => {
                self.messages = messages;
                self.replayed = true;
                Ok(())
            }
            Some(st3::mailbox::Frame::Fenced { reason }) => anyhow::bail!("{reason}"),
            None => anyhow::bail!("native mailbox subscription ended"),
        }
    }
    async fn pump(
        &mut self,
        client: &Client,
        agent_dir: &Path,
        receipts: NativeDeliveryReceipts<'_>,
    ) -> Result<()> {
        if !self.replayed {
            return Ok(());
        }
        let consumed = match receipts {
            NativeDeliveryReceipts::Codex {
                state_dir,
                identity,
                runtime_id,
            } => st_drivers::codex_app_server::consumed_delivery_filenames(
                state_dir, identity, runtime_id,
            )?,
            NativeDeliveryReceipts::OpenCode {
                session_dir,
                identity,
                runtime_id,
            } => st_drivers::opencode_session::consumed_delivery_paths(
                session_dir,
                identity,
                runtime_id,
            )?,
            _ => BTreeSet::new(),
        };
        let active: BTreeSet<_> = self
            .messages
            .iter()
            .filter(|view| matches!(view.status.as_str(), "sent" | "staged" | "delivered"))
            .map(|view| view.subject.clone())
            .collect();
        self.queued.retain(|key, _| active.contains(key));
        let mut first_error = None;
        for view in &self.messages {
            if !active.contains(&view.subject) {
                continue;
            }
            let result: Result<()> = async {
                if consumed.contains(&view.subject) {
                    if view.status != "delivered" {
                        mailbox_receipt(client, &self.fence, &view.subject, "delivered").await?;
                    }
                    mailbox_receipt(client, &self.fence, &view.subject, "read").await?;
                    return Ok(());
                }
                // Reoffer delivered-unread mail under its stable key. The provider ledger
                // reconciles native consumption and uncertain handoffs before any reinjection.
                if view.status == "sent" {
                    let staged =
                        mailbox_receipt_claim(client, &self.fence, &view.subject, "staged").await?;
                    if staged.kind != "message.staged" {
                        self.queued.remove(&view.subject);
                        return Ok(());
                    }
                }
                if !self.queued.contains_key(&view.subject) {
                    let body = message_content(client, view).await?;
                    // Files first: a message that names an image is queued once the image is here.
                    let attachments = st3::blobs::materialize_for_seat(
                        client,
                        &self.fence.subject,
                        &agent_dir.join("attachments"),
                        view,
                    )
                    .await?;
                    self.queued.insert(
                        view.subject.clone(),
                        native_queued_message(view, body, &attachments),
                    );
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        // Keep uncertain handoffs in the native ledger until their read acknowledgement lands.
        let mut queued: Vec<_> = self.queued.values().cloned().collect();
        queued.sort_by(|left, right| (left.ts_ms, &left.filename).cmp(&(right.ts_ms, &right.filename)));
        st_drivers::push_mailbox::replace_active(agent_dir, queued, active);
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }
}

fn native_queued_message(
    view: &MessageView,
    body: String,
    attachments: &[st_drivers::ding::AttachmentNotice],
) -> st_drivers::message::Message {
    let mut tags = vec![
        format!("st3-message:{}", view.subject),
        format!("{}{}", st_drivers::ding::ST3_TO_TAG, view.to),
        format!(
            "{}{}",
            st_drivers::ding::ST3_SHA256_TAG,
            st_drivers::ding::st3_body_sha256(&body)
        ),
    ];
    tags.extend(
        attachments
            .iter()
            .map(st_drivers::ding::AttachmentNotice::to_tag),
    );
    st_drivers::message::Message {
        filename: view.subject.clone(),
        ts_ms: view.created_index,
        from: Some(view.from.clone()),
        subject: view.title.clone(),
        in_reply_to: view.in_reply_to.clone(),
        tags,
        priority: None,
        idempotency_key: None,
        stream: None,
        event_id: None,
        event_key: None,
        body,
    }
}

async fn mailbox_receipt(
    client: &Client,
    fence: &st3::mailbox::Fence,
    message: &str,
    lifecycle: &str,
) -> Result<()> {
    mailbox_receipt_claim(client, fence, message, lifecycle).await?;
    Ok(())
}

async fn mailbox_receipt_claim(
    client: &Client,
    fence: &st3::mailbox::Fence,
    message: &str,
    lifecycle: &str,
) -> Result<ClaimRecord> {
    client
        .post(
            "/v1/mailbox/receipts",
            &st3::mailbox::Receipt {
                fence: fence.clone(),
                message: message.into(),
                lifecycle: lifecycle.into(),
            },
        )
        .await
}

fn seat_label(seat: &st3::model::DesiredSubject) -> String {
    st3::mailbox::seat_label(
        seat,
        std::env::var("AGENT_PERSONA_SHORT").ok().as_deref(),
    )
}
fn update_native_title(seat: &st3::model::DesiredSubject, runtime_id: &str) -> Result<()> {
    let label = seat_label(seat);
    let result = std::process::Command::new("pty")
        .args(["rename", runtime_id, &label])
        .output()?;
    anyhow::ensure!(
        result.status.success(),
        "updating the PTY title failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
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
    match forward_projected_messages_reporting(
        client,
        subject,
        inbox,
        archive,
        transport,
        receipts,
        supervisor.report.as_deref(),
    )
    .await
    {
        Ok(unforwarded) => {
            // A message that cannot be forwarded is recorded once on its own and retried with the
            // next poll. It never pauses delivery of the recipient's other messages.
            for (message, reason) in unforwarded {
                let _ = report_unforwarded_message(
                    client,
                    subject,
                    incarnation,
                    transport,
                    &message,
                    &reason,
                )
                .await;
            }
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

/// Forward the recipient's queued messages into its native inbox. Each message is forwarded on
/// its own; the ones that could not be forwarded are returned with their reasons.
#[cfg(test)]
async fn forward_projected_messages(
    client: &Client,
    subject: &str,
    inbox: &Path,
    archive: &Path,
    transport: &str,
    receipts: NativeDeliveryReceipts<'_>,
) -> Result<Vec<(String, String)>> {
    forward_projected_messages_reporting(client, subject, inbox, archive, transport, receipts, None)
        .await
}

/// [`forward_projected_messages`], attaching the driver's delivery report to its mailbox poll.
async fn forward_projected_messages_reporting(
    client: &Client,
    subject: &str,
    inbox: &Path,
    archive: &Path,
    transport: &str,
    receipts: NativeDeliveryReceipts<'_>,
    report: Option<&str>,
) -> Result<Vec<(String, String)>> {
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
        } => st_drivers::codex_app_server::consumed_delivery_filenames(state_dir, identity, runtime_id),
        NativeDeliveryReceipts::ClaudeChannel {
            agent_dir,
            incarnation,
        } => claude_channel_consumed_delivery_filenames(agent_dir, incarnation),
        NativeDeliveryReceipts::OpenCode {
            session_dir,
            identity,
            runtime_id,
        } => {
            st_drivers::opencode_session::consumed_delivery_paths(session_dir, identity, runtime_id)
        }
    }?;
    let mut cursor = None;
    let mut failures = Vec::new();
    loop {
        let page = message_page_reporting(
            client,
            Some(subject),
            false,
            cursor.as_deref(),
            if cursor.is_none() { report } else { None },
        )
        .await?;
        for message in page.items {
            active_subjects.insert(message.subject.clone());
            if matches!(message.status.as_str(), "read" | "closed") {
                consumed_by_recipient.insert(message.subject);
                continue;
            }
            if !matches!(message.status.as_str(), "sent" | "staged" | "delivered") {
                continue;
            }
            // One message that cannot be forwarded, such as a document missing on this host, is
            // reported without holding back the messages after it.
            let message_subject = message.subject.clone();
            let forwarded: Result<()> = async {
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
                    tags.push(format!("{}{}", st_drivers::ding::ST3_TO_TAG, message.to));
                    tags.push(format!(
                        "{}{}",
                        st_drivers::ding::ST3_SHA256_TAG,
                        st_drivers::ding::st3_body_sha256(&content)
                    ));
                    let filename = st_drivers::message::send_to_inbox(
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
                // merely queued native delivery. Delivered-unread mail is reoffered every poll, but
                // its delivery is already recorded: posting it again on each poll wakes every
                // mailbox reader in the daemon for nothing (#1085).
                if message.status == "delivered" || !native_delivery_receipted(&consumed, &filename)
                {
                    return Ok(());
                }
                deliver_message(
                    client,
                    &message.subject,
                    subject,
                    format!("native-delivered:{transport}:{subject}:{}", message.subject),
                )
                .await?;
                Ok(())
            }
            .await;
            if let Err(error) = forwarded {
                failures.push((message_subject, format!("{error:#}")));
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    // A message can close between polls. The active page intentionally excludes
    // history, so inspect only projected files still in the native inbox before
    // deciding whether to archive them. A failed lookup keeps the file in place.
    for file in st_drivers::message::list_dir(inbox)? {
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
    Ok(failures)
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
        session_dir: &'a Path,
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
        st_drivers::harness_timeline::read(&st_drivers::harness_timeline::timeline_path(agent_dir))
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
        .filter(|filename| st_drivers::message::is_message_filename(filename))
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
    for message in st_drivers::message::list_dir(inbox)? {
        let is_consumed = message
            .tags
            .iter()
            .filter_map(|tag| tag.strip_prefix(TAG_PREFIX))
            .any(|subject| consumed_by_recipient.contains(subject));
        if is_consumed {
            st_drivers::message::archive_msg(inbox, archive, &message.filename)?;
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
    Ok(st_drivers::message::list_dir(inbox)?
        .into_iter()
        .chain(st_drivers::message::list_dir(archive)?)
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
/// Apply this node's `[limits]` policy every two minutes: stop the seats it hosts on an account
/// past its weekly limit, and notify operations once per weekly window.
async fn enforce_account_limits(store: Arc<Store>, policy: st3::store::LimitsPolicy) {
    const LIMITS_INTERVAL: Duration = Duration::from_secs(2 * 60);
    loop {
        let pass = store.clone();
        let policy = policy.clone();
        match tokio::task::spawn_blocking(move || {
            st3::profile::task("task enforce-account-limits", || {
                pass.enforce_account_limits(&policy, now_ms())
            })
        })
        .await
        {
            Ok(Ok(outcome)) => {
                for seat in outcome.stopped {
                    eprintln!("st3: limits policy stopped {seat}");
                }
                for (seat, account) in outcome.switched {
                    eprintln!("st3: limits policy restarted {seat} on account {account}");
                }
            }
            Ok(Err(error)) => eprintln!("st3: limits policy failed: {error}"),
            Err(error) => eprintln!("st3: limits policy stopped: {error}"),
        }
        tokio::time::sleep(LIMITS_INTERVAL).await;
    }
}

async fn trim_local_observations(store: Arc<Store>, observations: st3::config::ObservationsConfig) {
    const LOCAL_OBSERVATION_TRIM_INTERVAL: Duration = Duration::from_secs(60 * 60);
    const LOCAL_OBSERVATION_TRIM_CHUNK: usize = 5_000;
    let retention_ms = observations
        .retention_ms()
        .expect("the daemon validated its observation retention");
    loop {
        let observation_store = store.clone();
        let max_per_subject_kind = observations.max_per_subject_kind;
        let trimmed = tokio::task::spawn_blocking(move || {
            st3::profile::task("task trim-local-observations", || {
                observation_store.trim_local_observations(
                    now_ms().saturating_sub(u128::from(retention_ms)),
                    max_per_subject_kind,
                    LOCAL_OBSERVATION_TRIM_CHUNK,
                )
            })
        })
        .await;
        let usage_store = store.clone();
        if let Ok(Err(error)) = tokio::task::spawn_blocking(move || {
            usage_store.trim_usage_responses(
                now_ms().saturating_sub(st3::store::USAGE_RESPONSE_HORIZON_MS),
                LOCAL_OBSERVATION_TRIM_CHUNK,
            )
        })
        .await
        {
            eprintln!("st3: usage response trim failed: {error:#}");
        }
        match trimmed {
            Ok(Ok(0)) => {}
            Ok(Ok(count)) => eprintln!("st3: trimmed {count} local observations"),
            Ok(Err(error)) => eprintln!("st3: local observation trim failed: {error:#}"),
            Err(error) => eprintln!("st3: local observation trim stopped: {error}"),
        }
        tokio::time::sleep(LOCAL_OBSERVATION_TRIM_INTERVAL).await;
    }
}

/// Seal and verify checkpoints every ten minutes. The proof copies the store and replays it, so
/// it runs on a blocking thread, and a copy left by a crash is removed first.
async fn run_checkpoints(store: Arc<Store>, context: st3::store::CheckpointContext) {
    const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(10 * 60);
    let _ = std::fs::remove_dir_all(&context.scratch);
    loop {
        let store = store.clone();
        let context = st3::store::CheckpointContext {
            now_unix_ms: now_ms(),
            ..context.clone()
        };
        match tokio::task::spawn_blocking(move || {
            st3::profile::task("task checkpoint", || store.checkpoint_step(&context))
        })
        .await
        {
            Ok(Ok(actions)) => {
                for action in actions {
                    eprintln!(
                        "st3: checkpoint {}",
                        serde_json::to_string(&action).unwrap_or_default()
                    );
                }
            }
            Ok(Err(error)) => eprintln!("st3: checkpoint work failed: {error:#}"),
            Err(error) => eprintln!("st3: checkpoint work stopped: {error}"),
        }
        tokio::time::sleep(CHECKPOINT_INTERVAL).await;
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

    #[test]
    fn harness_todo_graph_channel_rehydrates_and_spools_only_current_binding() {
        let root = tempfile::tempdir().unwrap();
        st_drivers::harness_events::enable(root.path(), "runtime-a").unwrap();
        let mut state = PiChannelResume {
            incarnation: "runtime-a".into(),
            todo_outbox: Some(root.path().into()),
            ..PiChannelResume::default()
        };
        let frame = json!({
            "type":"todo","session":"native-a","source_op":"hydrate",
            "observed_at":"2026-10-03T15:00:00Z","phases":[],
            "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":0},
            "truncated":false
        }).to_string();
        assert!(!state.accept_frame(&frame));
        assert!(state.accept_frame(r#"{"type":"ready","sessionId":"native-a"}"#));
        assert!(state.accept_frame(&frame));
        let events = st_drivers::harness_events::pending(root.path(), 100).unwrap();
        assert_eq!(events[0].payload["source_op"], "hydrate");
        assert_eq!(events[0].payload["phases"], json!([]));
        let mut resumed: PiChannelResume =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(resumed.accept_frame(r#"{"type":"session","sessionId":"native-b"}"#));
        assert!(!resumed.accept_frame(&frame));
        assert_eq!(st_drivers::harness_events::pending(root.path(), 100).unwrap().len(), 1);
    }

    #[test]
    fn harness_todo_spool_reconnect_preserves_sequence_and_next_incarnation_cleans_only_its_seat() {
        let root = tempfile::tempdir().unwrap();
        let current = prepare_channel_todo_outbox(root.path(), "agent/one", "runtime-a").unwrap();
        st_drivers::harness_events::enable(&current, "runtime-a").unwrap();
        let fields = json!({
            "harness":"omp", "incarnation_id":"runtime-a", "session_id":"native",
            "observed_at":"2026-10-03T15:00:00Z", "source_op":"hydrate", "phases":[],
            "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":0}, "truncated":false,
        });
        st_drivers::harness_events::write_channel_todo(&current, "runtime-a", &fields).unwrap();
        let sequence = st_drivers::harness_events::pending(&current, 10).unwrap()[0].sequence;
        let resumed = prepare_channel_todo_outbox(root.path(), "agent/one", "runtime-a").unwrap();
        assert_eq!(resumed, current);
        st_drivers::harness_events::write_channel_todo(&resumed, "runtime-a", &fields).unwrap();
        assert!(st_drivers::harness_events::pending(&resumed, 10).unwrap()[1].sequence > sequence);
        let unrelated = prepare_channel_todo_outbox(root.path(), "agent/two", "runtime-b").unwrap();
        st_drivers::harness_events::enable(&unrelated, "runtime-b").unwrap();
        prepare_channel_todo_outbox(root.path(), "agent/one", "runtime-c").unwrap();
        assert!(!current.exists());
        assert!(unrelated.exists());
        assert!(!todo_runtime_has_ended(&json!({"incarnation_id":"a","status":"running"}), "a"));
        assert!(todo_runtime_has_ended(&json!({"incarnation_id":"a","status":"exited"}), "a"));
        assert!(todo_runtime_has_ended(&json!({"incarnation_id":"b","status":"running"}), "a"));
        assert!(!todo_runtime_has_ended(&json!({}), "a"));
    }

    #[test]
    fn client_api_errors_print_in_plain_words_with_their_code() {
        let api = st3_client::ClientError::Api(
            st3_client::ErrorCode::StaleFence,
            "the client snapshot changed before the action was submitted".into(),
            Box::new(st3_client::ErrorEnvelope {
                api_version: "st3.client.v0".into(),
                error_version: "st3.client.error.v0".into(),
                request_id: "request/test".into(),
                code: st3_client::ErrorCode::StaleFence,
                message: "the client snapshot changed before the action was submitted".into(),
                retryable: false,
                retry_after_ms: None,
                details: Default::default(),
            }),
        );
        let error = anyhow::Error::new(api).context("start the terminal");
        assert_eq!(
            plain_error(&error),
            "start the terminal: st changed while this was on its way, so it was not applied (stale-fence)"
        );
        assert_eq!(plain_error(&anyhow::anyhow!("plain")), "plain");
    }

    #[test]
    fn a_claude_session_without_a_binding_is_reported_once_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let start = Instant::now();
        let mut watch = ClaudeBindingWatch::default();
        assert_eq!(watch.overdue(dir.path(), None, start), None);
        assert_eq!(watch.overdue(dir.path(), Some("wrapper-1"), start), None);
        let late = start + CLAUDE_BINDING_GRACE;
        let reason = watch.overdue(dir.path(), Some("wrapper-1"), late).unwrap();
        assert!(reason.contains("bound no native session"), "{reason}");
        assert_eq!(watch.overdue(dir.path(), Some("wrapper-1"), late), None);
        watch.retry();
        assert!(watch.overdue(dir.path(), Some("wrapper-1"), late).is_some());
        // A new wrapper session gets its own grace, and a bound one is never reported.
        assert_eq!(watch.overdue(dir.path(), Some("wrapper-2"), late), None);
        fs::write(
            dir.path().join(st3::hooks::CLAUDE_BINDING_FILE),
            r#"{"incarnation":"wrapper-2","native_session_id":"native-2"}"#,
        )
        .unwrap();
        assert_eq!(
            watch.overdue(dir.path(), Some("wrapper-2"), late + CLAUDE_BINDING_GRACE),
            None
        );
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn peer_status(peer: &str, digest: Option<&str>) -> st3::model::ReplicationPeerStatus {
        st3::model::ReplicationPeerStatus {
            projection_digests: Default::default(),
            differing_tables: Vec::new(),
            projection_comparison_waiting: false,
            peer: peer.into(),
            status: "up".into(),
            last_success_at_unix_ms: None,
            last_error: None,
            last_failure_at_unix_ms: None,
            refusal_reason: None,
            schema_digest: None,
            authority_digest: digest.map(str::to_owned),
            graph_digest: None,
            sync: None,
        }
    }

    #[test]
    fn a_wait_counts_only_exchanges_since_it_began() {
        let since = 10_000;
        let with = |name: &str, status: &str, measured: Option<(u128, u64)>| {
            let mut peer = peer_status(name, None);
            peer.status = status.into();
            peer.sync = measured.map(|(at, peer_only)| st3::model::ReplicationPeerSync {
                peer_only_envelopes: peer_only,
                measured_at_unix_ms: at,
                ..Default::default()
            });
            peer
        };
        let status = |peers| ReplicationStatus {
            peers,
            ..Default::default()
        };
        // An in-sync measurement from before the wait, as after a restart, proves nothing.
        let error = caught_up_since(&status(vec![with("alder", "up", Some((9_000, 0)))]), since)
            .unwrap_err();
        assert_eq!(error, "no exchange with alder yet");
        let error = caught_up_since(
            &status(vec![with("alder", "up", Some((11_000, 300)))]),
            since,
        )
        .unwrap_err();
        assert_eq!(error, "alder has 300 envelopes this node lacks");
        // A member that is not up cannot be checked, but some member must be.
        let caught_up = caught_up_since(
            &status(vec![
                with("alder", "up", Some((11_000, 0))),
                with("birch", "last-seen", Some((9_000, 0))),
            ]),
            since,
        )
        .unwrap();
        assert_eq!(caught_up.peers, vec![("alder".to_owned(), 11_000)]);
        assert_eq!(caught_up.not_checked, vec!["birch (last-seen)".to_owned()]);
        assert!(
            render_caught_up(&caught_up, 12_000)
                .contains("not checked, since they are not up: birch")
        );
        let error =
            caught_up_since(&status(vec![with("birch", "last-seen", None)]), since).unwrap_err();
        assert_eq!(error, "no peer has exchanged with this node");
    }

    #[test]
    fn mixed_build_status_reports_log_verification_and_waiting_projections() {
        let first = st3::model::ReplicationFirstSync {
            state: "verified".into(),
            authority_digest: Some("log-digest".into()),
            envelopes: Some(3),
            peer: Some("alder".into()),
            ..Default::default()
        };
        let rendered = render_first_sync(&first, 0);
        assert!(rendered.contains("log digest log-digest"));
        assert!(rendered.contains("projection comparison waits"));
        let mut peer = peer_status("alder", Some("log-digest"));
        peer.graph_digest = Some("another-graph".into());
        peer.projection_comparison_waiting = true;
        peer.sync = Some(Default::default());
        let rendered = render_replication_peers(&[peer], "local-graph", 0);
        assert!(rendered.contains("same envelopes"));
        assert!(rendered.contains("projection comparison waits for a newer build"));
        assert!(!rendered.contains("graphs differ"));
        assert!(!rendered.contains("diverged"));
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
    fn a_conversation_reads_as_stui_shows_it_and_raw_keeps_every_entry() {
        let items: Vec<ClientTimelineEntry> = serde_json::from_str(include_str!(
            "../../../fixtures/clients/transcripts/claude.json"
        ))
        .unwrap();
        let pretty = conversation_text(
            "session/example",
            &items,
            80,
            false,
            st3_conversation_ui::Density::Full,
        );
        assert!(pretty.starts_with("CONVERSATION  session/example"), "{pretty}");
        assert!(pretty.contains("Please check why the n"), "{pretty}");
        // The harness's own wrappers are cleaned away, as in stui; no escapes without colour.
        for noise in ["<task-notification>", "<channel", "<command-name>", "\x1b["] {
            assert!(!pretty.contains(noise), "{noise}: {pretty}");
        }
        let raw = timeline_entries_text("session/example", &items);
        assert!(raw.contains("<task-notification>"), "raw keeps what was stored");
    }

    #[test]
    fn usage_report_ranks_each_group_by_cost_and_marks_unpriced_tokens() {
        let report = json!({"rows": [
            {"agent":"agent/cheap","mission_run":"mission-run/one","step":"step-run/one/build","model":"model-a","account":"claude/aaaa","host":"host/a","cost_microusd":250000,"total_tokens":900,"input_tokens":200,"output_tokens":100,"cache_write_tokens":0,"cached_tokens":600,"unpriced_tokens":0},
            {"agent":"agent/dear","mission_run":"mission-run/two","step":"step-run/two/review","model":"model-b","account":"claude/bbbb","host":"host/b","cost_microusd":3000000,"total_tokens":30,"input_tokens":5,"output_tokens":2,"cache_write_tokens":3,"cached_tokens":20,"unpriced_tokens":0},
            {"agent":"agent/dear","mission_run":"mission-run/two","step":"step-run/two/review","model":"model-b","account":"claude/bbbb","host":"host/b","cost_microusd":1000000,"total_tokens":10,"input_tokens":2,"output_tokens":1,"cache_write_tokens":1,"cached_tokens":6,"unpriced_tokens":0},
            {"agent":"agent/local","mission_run":"","step":"","model":"model-local","account":"","host":"host/a","cost_microusd":0,"total_tokens":50,"input_tokens":50,"output_tokens":0,"cache_write_tokens":0,"cached_tokens":0,"unpriced_tokens":50},
        ]});
        let output = render_usage_report(&report, 24, None);
        assert_eq!(output.matches("USAGE  ").count(), 6);
        assert!(
            output.starts_with("SPEND  $4.25+ · 990 tokens · 24h · API-equivalent\n"),
            "{output}"
        );
        assert!(output.contains("UNPRICED  50 tokens on models without a price"));
        // Cost ranks, not tokens: the dear agent spent fewer tokens on a pricier model.
        assert!(
            output.find("$4.00  40  7  3  4  26  agent/dear").unwrap()
                < output
                    .find("$0.25  900  200  100  0  600  agent/cheap")
                    .unwrap()
        );
        assert!(output.contains("$0.00+  50  50  0  0  0  agent/local"));
        assert!(output.contains("$4.00  40  7  3  4  26  step-run/two/review"));
        assert!(output.contains("$4.00  40  7  3  4  26  claude/bbbb"));
        assert!(output.contains("$0.00+  50  50  0  0  0  unknown"));
        let with_limits = json!({"rows": [], "limits": [
            {"account": "claude/aaaa", "weekly_percent": 96.4, "five_hour_percent": null,
             "weekly_resets_at_unix_ms": 1_800_000_000_000_u64, "measured_at_unix_ms": 1_799_000_000_000_u64},
        ]});
        let output_with_limits = render_usage_report(&with_limits, 24, None);
        assert!(
            output_with_limits
                .contains("96%  ?  2027-01-15 08:00 UTC  2027-01-03 18:13 UTC  claude/aaaa"),
            "{output_with_limits}"
        );
        let by_step = render_usage_report(&report, 24, Some(UsageBy::Step));
        assert_eq!(by_step.matches("USAGE  ").count(), 1);
        assert!(by_step.contains("by step"));
    }

    #[test]
    fn declared_accounts_keep_response_spend_and_limits_separate_without_provider_identity() {
        let store = Store::open_memory("alder").unwrap();
        let mut labels = Vec::new();
        for (index, account) in ["ada/one", "ada/two"].into_iter().enumerate() {
            let subject = format!("agent/example/seat-{index}");
            let mut claim = ClaimInput {
                subject: subject.clone(), kind: "harness.timeline".into(), actor: Some(subject),
                fields: serde_json::from_value(json!({
                    "operation": "append", "entry_id": format!("response-{index}"), "source_id": format!("response-{index}"),
                    "sequence": 1, "revision": 1, "role": "system", "entry_type": "usage", "final": true,
                    "driver": "codex", "incarnation_id": "inc-one", "observed_at_unix_ms": now_ms() as u64,
                    "body": {"semantics": "response", "model": "example-model", "input_tokens": 10, "output_tokens": 2, "total_tokens": 12}
                })).unwrap(),
                evidence: Vec::new(), expected_subject: None, idempotency_key: None,
            };
            bind_observation_account(&mut claim, Some(account));
            labels.push(claim.fields["body"]["account"].clone());
            let observation = store.append_claim(&claim).unwrap();
            let rollup = store
                .usage_rollup_for_timeline(&observation)
                .unwrap()
                .unwrap();
            assert_eq!(rollup.fields["account"], labels[index]);
            assert_eq!(rollup.fields["total_tokens"], 12);
            store.append_claim(&rollup).unwrap();

            claim.kind = "harness.limits".into();
            claim.fields =
                serde_json::from_value(json!({"driver": "codex", "weekly_percent": 20 + index,
                "measured_at_unix_ms": now_ms() as u64}))
                .unwrap();
            bind_observation_account(&mut claim, Some(account));
            assert_eq!(claim.fields["account"], labels[index]);
            assert_eq!(claim.fields["account_ref"], account);
            store.append_claim(&claim).unwrap();
        }
        assert_ne!(labels[0], labels[1]);
        assert_eq!(store.account_limits().unwrap().len(), 2);
    }

    #[test]
    fn an_unbound_observation_keeps_the_provider_account() {
        let mut claim = ClaimInput {
            subject: "agent/example/seat".into(),
            kind: "harness.limits".into(),
            actor: None,
            fields: BTreeMap::from([
                ("driver".into(), json!("claude")),
                ("account".into(), json!("claude/provider-label")),
            ]),
            evidence: Vec::new(),
            expected_subject: None,
            idempotency_key: None,
        };
        let before = claim.fields.clone();
        bind_observation_account(&mut claim, None);
        assert_eq!(claim.fields, before);
    }

    #[test]
    fn usage_names_a_declared_account_beside_the_providers_label() {
        let report = json!({
            "rows": [
                {"agent":"agent/a","model":"m","account":"claude/aaaa","host":"host/a","cost_microusd":1000000,"total_tokens":10,"input_tokens":5,"output_tokens":5,"cache_write_tokens":0,"cached_tokens":0,"unpriced_tokens":0},
                {"agent":"agent/b","model":"m","account":"claude/bbbb","host":"host/a","cost_microusd":1000000,"total_tokens":10,"input_tokens":5,"output_tokens":5,"cache_write_tokens":0,"cached_tokens":0,"unpriced_tokens":0},
            ],
            "limits": [
                {"account": "claude/aaaa", "account_ref": "ada/claude-1", "weekly_percent": 40.0,
                 "measured_at_unix_ms": 1_799_000_000_000_u64},
                {"account": "claude/bbbb", "weekly_percent": 10.0,
                 "measured_at_unix_ms": 1_799_000_000_000_u64},
            ],
        });
        let output = render_usage_report(&report, 24, None);
        assert!(
            output.contains("  ada/claude-1 (claude/aaaa)\n"),
            "{output}"
        );
        assert!(output.contains("40%  ?"), "{output}");
        assert!(
            output.contains("  claude/bbbb\n"),
            "an account no declaration names keeps its label: {output}"
        );
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
            attachments: Vec::new(),
        };
        let omp = pi_family_message_frame(&message, "FACT QUARTZ", "run-1/wake.omp-2", &[]);
        assert_eq!(omp["deliverAs"], "steer");
        assert_eq!(
            omp["content"],
            format!(
                "<smalltalk-message id=\"0123456789abcdef\" from=\"agent/run-1/wake.claude\" \
                 to=\"agent/run-1/wake.omp-2\" subject=\"Cross-harness consensus: idle\" \
                 sha256=\"{}\" graph=\"message/0123456789abcdef\">\nFACT QUARTZ\n</smalltalk-message>",
                st_drivers::ding::st3_body_sha256("FACT QUARTZ")
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
            &[
                "st3",
                "gh",
                "watch",
                "acme/garden#12",
                "--as",
                "person/operator",
            ],
            &[
                "st3",
                "gh",
                "unwatch",
                "acme/garden#12",
                "--as",
                "agent/peer",
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
    fn a_claim_prints_each_host_document_under_its_reference() {
        let documents = [(
            "doc/hosts/example@abc".to_owned(),
            "# Example host\n\n- Services run through systemd.\n".to_owned(),
        )];
        assert_eq!(
            render_host_facts("example", &documents, OutputStyle::plain()),
            "\nHOST  example\n  doc/hosts/example@abc\n    # Example host\n\n    - Services run through systemd.\n"
        );
        assert_eq!(render_host_facts("example", &[], OutputStyle::plain()), "");
    }

    #[test]
    fn pi_family_session_context_carries_saved_context_and_no_instructions() {
        assert_eq!(pi_family_session_context("fleet/example/omp", " \n"), "");
        assert_eq!(
            pi_family_session_context("fleet/example/omp", "Resume the release notes.\n"),
            "<context source=\"st3/context/now.md\" agent=\"fleet/example/omp\">\nResume the release notes.\n</context>"
        );
    }

    #[test]
    fn a_pi_family_channel_carries_its_frames_across_a_replacement() {
        let mut state = PiChannelResume {
            incarnation: "1:one".into(),
            session: "session".into(),
            ..PiChannelResume::default()
        };
        state
            .lines
            .push(b"{\"type\":\"state\",\"state\":\"idle\"}\n{\"type\":\"deliv");
        while let Some(line) = state.lines.next_line() {
            assert!(state.accept_frame(&line));
        }
        assert!(state.first_idle_seen);
        assert_eq!(state.pending.state, Some(("idle".to_owned(), 1)));
        state.delivered.insert("message/one".into());

        // The next image reads everything back, including the partial frame.
        let mut resumed: PiChannelResume =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        resumed
            .lines
            .push(b"ered\",\"meta\":{\"messageId\":\"message/one\"}}\n");
        let line = resumed.lines.next_line().unwrap();
        assert!(resumed.accept_frame(&line));
        assert!(resumed.pending.acknowledgements.contains("message/one"));
        assert!(resumed.delivered.contains("message/one"));
        assert_eq!(resumed.frame_sequence, 1);
        // A handoff failure below the retry limit makes the message deliverable again.
        assert!(!resumed.accept_frame(r#"{"type":"failed","meta":{"messageId":"message/one"}}"#));
        assert!(!resumed.delivered.contains("message/one"));
    }

    #[tokio::test]
    async fn native_mailbox_replays_delivered_unread_but_never_queues_read_or_closed_mail()
    {
        // The two providers share this pump but load their own native receipt ledgers.
        for driver in ["codex", "opencode"] {
            let root = tempfile::tempdir().unwrap();
            let agent_dir = root.path().join("agent");
            std::fs::create_dir_all(&agent_dir).unwrap();
            st_drivers::push_mailbox::register(&agent_dir);
            let client = Client::new(Endpoint::Unix(root.path().join("absent-daemon.sock")));
            let mut view = MessageView {
                subject: "message/legacy".into(),
                from: "person/eval".into(),
                to: "agent/eval.worker".into(),
                content: "Recovered signal".into(),
                status: "delivered".into(),
                title: None,
                in_reply_to: None,
                tags: Vec::new(),
                created_index: 1,
                attachments: Vec::new(),
            };
            let mut mailbox = NativeMailbox {
                subscription: None,
                fence: st3::mailbox::Fence::new(&view.to, "new-incarnation", "delivery"),
                messages: vec![view.clone()],
                queued: BTreeMap::new(),
                replayed: true,
            };
            view.subject = "message/pending".into();
            view.status = "staged".into();
            view.content = "Fresh signal".into();
            mailbox.messages.push(view.clone());
            for status in ["read", "closed"] {
                let mut settled = view.clone();
                settled.subject = format!("message/{status}");
                settled.status = status.into();
                mailbox.messages.push(settled);
            }
            for _ in 0..3 {
                let receipts = if driver == "codex" {
                    NativeDeliveryReceipts::Codex {
                        state_dir: root.path(),
                        identity: "eval.worker",
                        runtime_id: "worker",
                    }
                } else {
                    NativeDeliveryReceipts::OpenCode {
                        session_dir: root.path(),
                        identity: "eval.worker",
                        runtime_id: "worker",
                    }
                };
                // Replay requires no backward staging or invented consumption receipt.
                mailbox.pump(&client, &agent_dir, receipts).await.unwrap();
                let queued = st_drivers::push_mailbox::messages(
                    &agent_dir,
                    &agent_dir.join("resources/inbox"),
                )
                .unwrap();
                let mut ids = queued.iter().map(|message| message.filename.as_str()).collect::<Vec<_>>();
                ids.sort_unstable();
                assert_eq!(ids, ["message/legacy", "message/pending"], "{driver}");
            }
            assert!(!agent_dir.join("resources").exists());
        }
    }

    #[tokio::test]
    async fn pi_family_pending_read_survives_daemon_outage_and_reexec_under_its_fence() {
        use axum::{Json, Router, routing::post};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("daemon.sock");
        let client = Client::new(st3::client::Endpoint::Unix(path.clone()));
        let mut fence = st3::mailbox::Fence::new("agent/eval.worker", "session-1", "delivery");
        fence.epoch = 7; // The daemon's already-allocated binding, carried through exec.
        let mut state = PiChannelResume {
            incarnation: "session-1".into(),
            session: "native-session".into(),
            pending: PiFamilyReports {
                fence: Some(fence.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        state.accept_frame(r#"{"type":"read","meta":{"messageId":"message/native"}}"#);
        assert!(
            state
                .pending
                .publish(
                    &client,
                    &fence.subject,
                    "omp",
                    &fence.incarnation,
                    "native-session"
                )
                .await
                .is_err()
        );
        assert!(state.pending.reads.contains("message/native"));
        assert!(state.pending.acknowledgements.contains("message/native"));
        let resume_path =
            st_drivers::reexec::write_state(root.path(), "channel-resume", &state).unwrap();
        let mut resumed: PiChannelResume = st_drivers::reexec::read_state(&resume_path).unwrap();
        assert_eq!(
            serde_json::to_value(resumed.pending.fence.as_ref().unwrap()).unwrap(),
            serde_json::to_value(&fence).unwrap()
        );
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = received.clone();
        let expected = serde_json::to_value(&fence).unwrap();
        let store = std::sync::Arc::new(
            st3::store::Store::open(&root.path().join("graph.db"), "node").unwrap(),
        );
        store
            .append_claim(&ClaimInput {
                subject: "message/native".into(),
                kind: "message.sent".into(),
                actor: Some("person/eval".into()),
                fields: BTreeMap::from([
                    ("status".into(), json!("sent")),
                    ("from".into(), json!("person/eval")),
                    ("to".into(), json!(fence.subject)),
                    ("content".into(), json!("QUARTZ SIGNAL")),
                ]),
                evidence: Vec::new(),
                expected_subject: None,
                idempotency_key: Some("read-resume-send".into()),
            })
            .unwrap();
        let graph = store.clone();
        let app = Router::new().route(
            "/v1/mailbox/receipts",
            post(move |Json(receipt): Json<st3::mailbox::Receipt>| {
                let received = captured.clone();
                let expected = expected.clone();
                let store = graph.clone();
                async move {
                    assert_eq!(serde_json::to_value(&receipt.fence).unwrap(), expected);
                    received.lock().unwrap().push(receipt.lifecycle.clone());
                    let record = store
                        .append_claim(&ClaimInput {
                            subject: receipt.message,
                            kind: format!("message.{}", receipt.lifecycle),
                            actor: Some(receipt.fence.subject),
                            fields: BTreeMap::from([("status".into(), json!(receipt.lifecycle))]),
                            evidence: Vec::new(),
                            expected_subject: None,
                            idempotency_key: None,
                        })
                        .unwrap();
                    Json(json!({"api_version":"st3.v1", "value":record}))
                }
            }),
        );
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            st3::api::serve_unix(&server_path, app).await.unwrap();
        });
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        resumed
            .pending
            .publish(
                &client,
                &fence.subject,
                "omp",
                &fence.incarnation,
                "native-session",
            )
            .await
            .unwrap();
        assert!(resumed.pending.reads.is_empty());
        assert!(resumed.pending.acknowledgements.is_empty());
        assert_eq!(*received.lock().unwrap(), vec!["delivered", "read"]);
        assert_eq!(
            store.message("message/native").unwrap().unwrap().status,
            "read"
        );
        assert!(!root.path().join("resources").exists());
        server.abort();
    }
    #[test]
    fn a_human_ask_survives_channel_replacement_until_an_answered_state_frame() {
        let mut state = PiChannelResume::default();
        assert!(state.accept_frame(
            r#"{"type":"state","state":"active","blockedOn":"human","ask":"question","reason":"Which deployment target?"}"#,
        ));
        let mut resumed: PiChannelResume =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(resumed.pending.state, Some(("working".into(), 1)));
        assert_eq!(resumed.pending.blocked_on.as_deref(), Some("human"));
        assert_eq!(resumed.pending.ask.as_deref(), Some("question"));
        assert_eq!(resumed.pending.reason.as_deref(), Some("Which deployment target?"));
        // The extension only emits an unblocked state for the matching ask's result.
        // An unrelated tool result is timeline data, not a new harness observation.
        assert!(!resumed.accept_frame(
            r#"{"type":"timeline","event":"tool_result","payload":{"toolCallId":"unrelated"}}"#,
        ));
        assert_eq!(resumed.pending.blocked_on.as_deref(), Some("human"));
        assert!(resumed.accept_frame(r#"{"type":"state","state":"active"}"#));
        assert_eq!(resumed.pending.state, Some(("working".into(), 2)));
        assert!(resumed.pending.blocked_on.is_none());
        assert!(resumed.pending.ask.is_none());
        assert!(resumed.pending.reason.is_none());
    }

    #[test]
    fn repeated_negative_handoffs_keep_retrying_and_resume_the_backoff() {
        let mut state = PiChannelResume::default();
        for _ in 0..5 {
            state.delivered.insert("message/retry".into());
            state.accept_frame(r#"{"type":"failed","meta":{"messageId":"message/retry"}}"#);
            assert!(!state.delivered.contains("message/retry"));
            assert!(state.retry_after_ms.contains_key("message/retry"));
        }
        assert!(state.failed_diagnostics.contains("message/retry"));
        let mut resumed: PiChannelResume =
            serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(state.retry_after_ms, resumed.retry_after_ms);
        resumed.accept_frame(r#"{"type":"delivered","meta":{"messageId":"message/retry"}}"#);
        assert!(!resumed.retry_after_ms.contains_key("message/retry"));
        assert!(!resumed.failed_diagnostics.contains("message/retry"));
        assert!(!resumed.failed_handoffs.contains_key("message/retry"));
    }

    #[test]
    fn a_driver_resume_state_names_its_driver_session_and_loop() {
        let resume = DriverResume {
            driver: "claude".into(),
            subject: "agent/example/worker".into(),
            incarnation: "1:one".into(),
            session: st_drivers::provider_session::DetachedSession::Provider {
                pid: 42,
                session: "token".into(),
                seq: 3,
            },
            loop_state: NativeLoopState {
                paths: None,
                mailbox_fence: None,
                ready: true,
                harness_record_started: true,
                predecessor_harness_record: Some(b"ignored".to_vec()),
                published_timeline: BTreeSet::from(["one:1:1:upsert".to_owned()]),
                delivery_episode: 2,
            },
        };
        let back: DriverResume =
            serde_json::from_slice(&serde_json::to_vec(&resume).unwrap()).unwrap();
        assert_eq!(back.session, resume.session);
        assert_eq!(back.incarnation, resume.incarnation);
        assert!(back.loop_state.ready);
        assert_eq!(back.loop_state.delivery_episode, 2);
        assert_eq!(
            back.loop_state.published_timeline,
            resume.loop_state.published_timeline
        );
        // A predecessor's harness record is only a startup fence; it never crosses an exec.
        assert!(back.loop_state.predecessor_harness_record.is_none());
    }

    #[test]
    fn a_delivery_report_names_the_process_and_its_channel() {
        let root = tempfile::tempdir().unwrap();
        let report: Value =
            serde_json::from_str(&native_delivery_report("claude-channel", Some(root.path())))
                .unwrap();
        assert_eq!(report["transport"], "claude-channel");
        assert_eq!(report["pid"], std::process::id());
        assert!(report["image"].is_string());
        assert!(report["channel"].is_null(), "no channel has reported yet");
        let codex: Value =
            serde_json::from_str(&native_delivery_report("app-server", None)).unwrap();
        assert!(codex.get("channel").is_none());
    }

    #[test]
    fn agent_card_todo_preserves_snapshot_and_reports_active_blocked_empty_truncated_stale() {
        let mut agent: st3_client::Agent = serde_json::from_value(serde_json::json!({
            "kind":"agent", "id":"agent/worker", "revision":"one",
            "updated_at":"2026-10-03T09:00:00Z", "name":"Worker",
            "state":"running", "reachability":"local", "runtime_ids":[],
            "todo": {
                "claim_id":"claim/todo", "accepted_at":"2026-10-03T09:00:00Z", "stale":false,
                "snapshot": {
                    "harness":"omp", "session_id":"native", "incarnation_id":"one",
                    "observed_at":"2026-10-03T09:00:00Z", "source_op":"update",
                    "phases":[{"name":"Build", "tasks":[
                        {"content":"Compile", "status":"in_progress", "blocker":null},
                        {"content":"Deploy", "status":"blocked", "blocker":"Approval"},
                        {"content":"Later", "status":"in_progress", "blocker":null}
                    ]}],
                    "totals":{"pending":4,"in_progress":2,"completed":3,"blocked":1},
                    "truncated":true
                }
            }
        })).unwrap();
        let card = render_client_agent(&agent, &[], 0);
        assert!(card.contains("Todo         ▶ Compile · 3/10 done · 1 blocked · truncated\n"));
        assert!(!card.contains("▶ Later"));
        let value = serde_json::to_value(&agent).unwrap();
        assert_eq!(value["todo"]["snapshot"]["phases"][0]["tasks"][1]["blocker"], "Approval");
        assert_eq!(value["todo"]["claim_id"], "claim/todo");
        agent.todo.as_mut().unwrap().snapshot.phases[0].tasks[0].content =
            "\u{1b}[31mCompile\u{1b}[0m\nnext\tstep\u{1b}]0;spoofed title\u{7}\u{8}".into();
        assert!(render_client_agent(&agent, &[], 0)
            .contains("Todo         ▶ Compile next step · 3/10 done · 1 blocked · truncated\n"));
        agent.todo.as_mut().unwrap().snapshot.totals.abandoned = 2;
        assert!(render_client_agent(&agent, &[], 0)
            .contains("3/10 done · 1 blocked · 2 abandoned · truncated"));
        let todo = agent.todo.as_mut().unwrap();
        todo.stale = true;
        todo.snapshot.phases.clear();
        todo.snapshot.truncated = false;
        todo.snapshot.totals = st3_client::HarnessTodoTotals {
            pending: 0, in_progress: 0, completed: 0, blocked: 0, abandoned: 0,
        };
        assert!(render_client_agent(&agent, &[], 0).contains("Todo         0/0 done · 0 blocked · stale\n"));
        agent.todo = None;
        assert!(!render_client_agent(&agent, &[], 0).contains("Todo"));
        assert!(serde_json::to_value(agent).unwrap()["todo"].is_null());
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

    fn subagent_worker() -> serde_json::Value {
        serde_json::json!({
            "kind": "agent", "id": "agent/crew/worker", "revision": "one",
            "updated_at": "2026-10-02T09:00:00Z", "name": "Worker",
            "state": "running", "reachability": "local", "runtime_ids": [],
            "owner_run_id": "mission-run/crew", "harness_state": "working",
            "subagents": [
                {"id": "a1", "subagent_type": "Explore", "description": "map the code",
                 "driver": "claude", "work_id": "step-run/crew/build",
                 "started_at": "2026-10-02T09:00:00Z",
                 "lease_expires_at": "2026-10-02T09:10:00Z"},
                {"id": "019a-thread", "subagent_type": null, "description": null,
                 "driver": "codex", "started_at": null,
                 "lease_expires_at": "2026-10-02T09:10:00Z"}
            ]
        })
    }

    #[test]
    fn agent_card_lists_the_subagents_its_harness_runs() {
        let st3_client::Resource::Agent(agent) = serde_json::from_value(subagent_worker()).unwrap()
        else {
            panic!("agent resource")
        };
        let started = 1_790_931_600_000_u128;
        let card = render_client_agent(&agent, &[], started + 180_000);
        assert!(
            card.contains(
                "SUBAGENT     map the code · Explore · started 3m ago\nSUBAGENT     019a-thread\n"
            ),
            "{card}"
        );
        // An agent without subagents prints no subagent line.
        let quiet = render_client_agent(
            &st3_client::Agent {
                subagents: Vec::new(),
                ..agent
            },
            &[],
            started,
        );
        assert!(!quiet.contains("SUBAGENT"), "{quiet}");
    }

    #[test]
    fn agent_tree_nests_running_subagents_under_their_agent() {
        let page: ClientPage = serde_json::from_value(serde_json::json!({
            "kind": "page", "collection": "agents",
            "items": [subagent_worker()],
            "page": {"limit": 50, "has_more": false}
        }))
        .unwrap();
        let tree = render_client_agents(&page, true, false, "st agents tree");
        assert_eq!(
            tree,
            "AGENT TREE  1\n\
             └─ crew\n\
             \x20  └─ worker  working · local\n\
             \x20     agent/crew/worker\n\
             \x20     ├─ map the code · Explore\n\
             \x20     └─ 019a-thread\n"
        );
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
            "kind": "agent-queue", "agent_id": "agent/example/worker",
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
            "AGENT QUEUE  agent/example/worker\n\
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

        // The recipient's late acknowledgement settles to its existing read evidence.
        deliver_message(&client, "message/held", seat, "late".into())
            .await
            .unwrap();
        // Receipt replay must not end the channel or cause another handoff.
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

    #[tokio::test]
    async fn trace_wait_starts_after_existing_events() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("st3.sock");
        let store = Arc::new(Store::open_memory("wait-cursor-test").unwrap());
        let mut last_index = 0;
        for number in 0..3 {
            last_index = store
                .append_claim(&ClaimInput {
                    subject: "host/wait-cursor-test".into(),
                    kind: "transport.observed".into(),
                    actor: None,
                    fields: BTreeMap::from([("status".into(), Value::String("up".into()))]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: Some(format!("wait-cursor-test-{number}")),
                })
                .unwrap()
                .store_index;
        }
        let state = AppState {
            store,
            notify: Arc::new(Notify::new()),
            event_notify: watch::channel(0_u64).0,
            node: "wait-cursor-test".into(),
            state_dir: root.path().to_path_buf(),
            pty_root: root.path().join("pty"),
            pty_binary: PathBuf::from("pty"),
            fleet_id: None,
            configured_peers: Vec::new(),
            client_relay: None,
            native_session_home: None,
            planner_default: PlannerSpec::default(),
        };
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let app = router(state).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let sent = sent.clone();
                async move {
                    if request.uri().path() == "/v1/events" {
                        let _ = sent.send(request.uri().query().unwrap_or_default().to_owned());
                    }
                    next.run(request).await
                }
            },
        ));
        let server = tokio::spawn(async move {
            serve_unix(&socket, app).await.unwrap();
        });
        let path = root.path().join("st3.sock");
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let client = Client::unix(&path);
        let waiter = tokio::spawn(async move {
            wait_for_condition(&client, "host/wait-cursor-test", "completed", None).await
        });
        let query = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .expect("the wait did not request events")
            .expect("the server stopped before the event request");
        let after = query
            .split('&')
            .find_map(|part| part.strip_prefix("after="))
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(
            after >= last_index,
            "the wait replayed existing events: {query}"
        );
        waiter.abort();
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
            replicated: None,
        }
    }

    #[test]
    fn replication_status_names_shared_table_differences() {
        let mut peer = peer_status("alder", None);
        peer.projection_digests
            .insert("planning_sessions".into(), "different".into());
        peer.differing_tables = vec!["documents".into(), "planning_sessions".into()];
        let rendered = render_replication_peers(&[peer], "local", 0);
        assert!(rendered.contains("shared tables differ: documents, planning_sessions"));
    }

    #[test]
    fn replication_status_renders_the_fabric_grant_reason() {
        let mut peer = peer_status("cobalt", None);
        peer.status = "refused".into();
        peer.refusal_reason =
            Some("refused by that member's Fabric grants (service st3-peer-v1)".into());
        let output = render_replication_peers(&[peer], "local", 2000);
        assert!(output.contains("peer\tcobalt\trefused\trefused by that member's Fabric grants"));
        assert!(!output.contains("down"));
    }

    #[test]
    fn replication_status_says_which_side_holds_what_and_how_long_catching_up_takes() {
        let now = 1_000_000;
        let local = "1111111111111111aaaa";
        let peer =
            |name: &str, graph: Option<&str>, sync: Option<st3::model::ReplicationPeerSync>| {
                ReplicationPeerStatus {
                    projection_digests: BTreeMap::from([("claim_sources".into(), "sample".into())]),
                    differing_tables: Vec::new(),
                    projection_comparison_waiting: false,
                    peer: name.into(),
                    status: "up".into(),
                    last_success_at_unix_ms: Some(now - 2_000),
                    last_error: None,
                    last_failure_at_unix_ms: None,
                    refusal_reason: None,
                    schema_digest: None,
                    authority_digest: None,
                    graph_digest: graph.map(str::to_owned),
                    sync,
                }
            };
        let same_envelopes = |compared: Option<u128>, differs: Option<u128>, diverged| {
            Some(st3::model::ReplicationPeerSync {
                measured_at_unix_ms: now,
                graph_compared_at_unix_ms: compared,
                graph_differs_since_unix_ms: differs,
                diverged,
                ..Default::default()
            })
        };
        let output = render_replication_peers(
            &[
                peer(
                    "ExampleMac",
                    Some("3333333333333333"),
                    Some(st3::model::ReplicationPeerSync {
                        peer_only_envelopes: 124_384,
                        local_only_envelopes: 3,
                        measured_at_unix_ms: now - 2_000,
                        receive_rate_per_second: Some(142.5),
                        catch_up_rate_per_second: Some(140.0),
                        estimated_catch_up_seconds: Some(889),
                        catching_up: true,
                        ..Default::default()
                    }),
                ),
                peer("Quiet", Some(local), same_envelopes(Some(now), None, false)),
                peer(
                    "Moved",
                    Some("4444444444444444"),
                    same_envelopes(Some(now), None, false),
                ),
                peer(
                    "Settling",
                    Some("5555555555555555"),
                    same_envelopes(Some(now), Some(now - 10_000), false),
                ),
                peer(
                    "Laptop",
                    Some("2222222222222222bbbb"),
                    same_envelopes(Some(now - 1_000), Some(now - 180_000), true),
                ),
                peer("Fresh", None, None),
            ],
            local,
            now,
        );
        assert_eq!(
            output,
            "sync\tdiverged: Laptop holds the same envelopes but projects a different graph, \
             since 3m ago\n\
             sync\tcatching up: ExampleMac has 124,384 envelopes this node lacks, \
             caught up in about 15m\n\
             peer\tExampleMac\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 ExampleMac has 124,384 envelopes this node lacks\n\
             \x20 this node has 3 envelopes ExampleMac lacks\n\
             \x20 receiving 142.5 envelopes/s, caught up in about 15m (measured 2s ago)\n\
             peer\tQuiet\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 in sync: the same envelopes and the same graph (measured now)\n\
             peer\tMoved\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 same envelopes (measured now), but the graphs differ (this node \
             111111111111, Moved 444444444444); the next exchange compares them\n\
             peer\tSettling\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 graphs differ: the same envelopes project different graphs since 10s ago \
             (compared now; this node 111111111111, Settling 555555555555)\n\
             \x20 diverged if this lasts a minute; a peer still projecting settles by itself\n\
             peer\tLaptop\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 diverged: the same envelopes project different graphs since 3m ago \
             (compared 1s ago; this node 111111111111, Laptop 222222222222)\n\
             \x20 exchanges cannot fix this; the nodes heal by comparing the claims each projects, and views on one node are wrong until then\n\
             peer\tFresh\tup\t\n\
             \x20 last seen 2s ago\n\
             \x20 difference not measured yet\n"
        );
    }

    #[test]
    fn a_catching_up_page_leads_with_how_far_behind_this_host_is() {
        let mut page = fixture_product_page(&["attention"], false);
        page.sync = Some(st3_client::SyncNotice {
            state: "catching-up".into(),
            peers: vec![st3_client::SyncPeer {
                host_id: "host/ExampleMac".into(),
                peer_only_envelopes: 1,
                local_only_envelopes: 0,
                last_exchange_at: Some("1970-01-01T00:16:38Z".into()),
                estimated_catch_up_seconds: None,
                diverged_since: None,
            }],
        });
        let output = render_now_page(&page, "st3 now --as person/alex");
        assert!(
            output.starts_with(
                "SYNCING  ExampleMac has 1 envelope this host lacks · estimating time to catch up · \
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
            "SYNCING  ExampleMac has 1 envelope this host lacks · estimating time to catch up · \
             last exchange 2s ago\n  Until then, items below can be out of date. \
             Progress: st3 replication status\n\n"
        );
        assert_eq!(catch_up_estimate(Some(0)), "caught up");
        assert_eq!(catch_up_estimate(Some(59)), "caught up in under a minute");
        assert_eq!(catch_up_estimate(Some(3_601)), "caught up in about 1h 1m");
        assert_eq!(catch_up_estimate(Some(90_000)), "caught up in about 1d 1h");
        assert_eq!(envelope_count(1_234_567), "1,234,567 envelopes");

        // A diverged peer outranks catching up: exchanges cannot fix what the page shows.
        let sync = page.sync.as_mut().unwrap();
        sync.state = "diverged".into();
        sync.peers[0].diverged_since = Some("1970-01-01T00:13:40Z".into());
        assert_eq!(
            render_sync_notice(page.sync.as_ref().unwrap(), 1_000_000),
            "DIVERGED  ExampleMac projects a different graph from the same envelopes · since 3m ago\n\
             \x20 Exchanges cannot fix this, so items below can be wrong. \
             Details: st3 replication status\n\n"
        );

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
        // A bounded recent-run window must not replace the daemon's complete run count.
        mission.total_runs = Some(100);
        mission.active_runs = Some(1);
        mission.runs.truncate(2);
        assert_eq!(render_mission_runs(mission), "1 active · 99 finished");
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
            rendered.contains("  requester retired: only person/alex can close it\n"),
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
                "  action: st attention show launch/release --as person/alex\n",
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
            "st now --as person/alex",
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
            "st now --as person/alex --owner-run mission-run/release/1",
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
            render_mailbox("person/alex", None, false, &[]),
            "MESSAGES  0\nFILTERS  mailbox=person/alex\nNo current items.\n"
        );
        assert_eq!(
            render_mailbox(
                "agent/worker",
                Some("person/alex"),
                true,
                &["message/one\tread\tperson/alex\tHello".into()]
            ),
            concat!(
                "MESSAGES  1\n",
                "FILTERS  mailbox=agent/worker · from=person/alex · archived=included\n",
                "message/one\tread\tperson/alex\tHello\n",
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
    fn legacy_attention_help_directs_callers_to_person_work() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("attention")
            .unwrap()
            .find_subcommand_mut("request")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(help.contains("attention-migrated"), "{help}");
        assert!(help.contains("work ask"), "{help}");
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
            "st attention ls --as person/alex"
        );
        assert_eq!(
            contract["purposes"]["attention"]["json_example"],
            "st attention ls --as person/alex --json"
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
                "st devices --as person/alex"
            ),
            concat!(
                "DEVICES  1\n",
                "device/ios-release  active  person/alex/session/ios-release  scopes 4\n",
                "  action: st devices --as person/alex revoke device/ios-release\n",
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
            "agent/example/app-web/standing/app-web",
        ])
        .unwrap();
        let Command::Terminals {
            command: PtyCommand::Attach(args),
        } = cli.command
        else {
            panic!("the PTY attach command did not parse");
        };
        assert_eq!(args.subject, "agent/example/app-web/standing/app-web");
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
            "terminal/agent/example/app-web/standing/app-web",
            "--as",
            "person/alex",
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
            "terminal/agent/example/app-web/standing/app-web"
        );
        assert_eq!(args.person.as_deref(), Some("person/alex"));
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
            "terminal/agent/example/app-web/standing/app-web",
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
            "terminal/agent/example/app-web/standing/app-web",
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
            "terminal/agent/example/app-web/standing/app-web",
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
            "agent/example/app-web/standing/app-web",
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
    fn every_offered_cli_command_has_an_action_coverage_row() {
        fn walk(
            command: &clap::Command,
            prefix: &str,
            output: &mut std::collections::BTreeSet<String>,
        ) {
            let children = command
                .get_subcommands()
                .filter(|child| !child.is_hide_set() && child.get_name() != "help")
                .collect::<Vec<_>>();
            if prefix != "st" && (children.is_empty() || !command.is_subcommand_required_set()) {
                output.insert(prefix.to_owned());
            }
            for child in children {
                walk(child, &format!("{prefix} {}", child.get_name()), output);
            }
        }
        let mut command = Cli::command();
        command.build();
        let mut offered = std::collections::BTreeSet::new();
        walk(&command, "st", &mut offered);
        let inventory: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/st3/action-coverage.json")).unwrap();
        let documented = inventory["cli"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["command"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            offered, documented,
            "Update the inventory and its end-to-end coverage when the CLI changes"
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
            "agent/example/cos/standing/cos",
        ])
        .unwrap();
        let Command::Missions {
            command: MissionViewCommand::Publish(args),
        } = cli.command
        else {
            panic!("the mission publish command did not parse");
        };
        assert_eq!(args.file, PathBuf::from("missions/typecase.kdl"));
        assert_eq!(args.actor, "agent/example/cos/standing/cos");
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
        for actor in ["agent/example/operator", "person/alex"] {
            assert!(
                Cli::try_parse_from([
                    "st3",
                    "missions",
                    "cancel",
                    "mission-run/example/one",
                    "--reason",
                    "superseded",
                    "--as",
                    actor
                ])
                .is_ok()
            );
        }
        assert!(
            Cli::try_parse_from([
                "st3",
                "missions",
                "cancel",
                "mission-run/example/one",
                "--reason",
                "superseded",
                "--as",
                "operator"
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from([
            "st3",
            "missions",
            "cancel",
            "mission-run/release/demo",
            "--reason",
            "the run was superseded",
            "--as",
            "person/alex",
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
        assert_eq!(args.actor, "person/alex");
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
            harness_activity_state(st_drivers::harness_state::Activity::Ended),
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
        let mut writer = st_drivers::harness_timeline::Writer::new(root.path(), "claude", "inc-2");
        writer
            .append(
                "prompt-1",
                st_drivers::harness_timeline::Role::User,
                st_drivers::harness_timeline::EntryType::Content,
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
            st_drivers::harness_timeline::Writer::new(root.path(), "claude", "provider-current");
        writer
            .append(
                "prompt-1",
                st_drivers::harness_timeline::Role::User,
                st_drivers::harness_timeline::EntryType::Content,
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
    fn message_send_and_reply_accept_a_key_for_unconfirmed_retries() {
        let send = Cli::try_parse_from([
            "st3",
            "conversations",
            "send",
            "agent/example/worker",
            "--from",
            "person/ada",
            "--body",
            "Hello",
            "--idempotency-key",
            "retry-a-send",
        ])
        .unwrap();
        let Command::Conversations {
            command: MessageCommand::Send(send),
        } = send.command
        else {
            panic!("send did not parse");
        };
        assert_eq!(send.idempotency_key.as_deref(), Some("retry-a-send"));
        let reply = Cli::try_parse_from([
            "st3",
            "conversations",
            "reply",
            "message/example",
            "--from",
            "person/ada",
            "--body",
            "Hello",
            "--idempotency-key",
            "retry-a-reply",
        ])
        .unwrap();
        let Command::Conversations {
            command: MessageCommand::Reply(reply),
        } = reply.command
        else {
            panic!("reply did not parse");
        };
        assert_eq!(reply.idempotency_key.as_deref(), Some("retry-a-reply"));

        let status = Cli::try_parse_from([
            "st3",
            "conversations",
            "status",
            "--idempotency-key",
            "retry-a-send",
        ])
        .unwrap();
        let Command::Conversations {
            command: MessageCommand::Status(status),
        } = status.command
        else {
            panic!("status did not parse");
        };
        assert_eq!(status.idempotency_key.as_deref(), Some("retry-a-send"));
        assert!(status.reference.is_none());
        assert!(Cli::try_parse_from(["st3", "conversations", "status"]).is_err());
        assert!(
            Cli::try_parse_from([
                "st3",
                "conversations",
                "status",
                "message/example",
                "--idempotency-key",
                "retry-a-send",
            ])
            .is_err()
        );
    }

    #[test]
    fn a_derived_message_key_names_one_message_in_one_hour() {
        let request = MessageSendRequest {
            idempotency_key: String::new(),
            from: "person/avery".into(),
            to: "agent/example/worker".into(),
            content: "The merge train is live.".into(),
            title: Some("Merge train".into()),
            in_reply_to: None,
            tags: Vec::new(),
            attachments: Vec::new(),
        };
        let key = derived_message_key(&request, None, 490_000);
        // The key never changes between builds: a retry after an upgrade finds the first send.
        assert_eq!(key, "st3-message:v1:31bd263335cbfc5fa34d30ccafe56f43");
        assert_eq!(key, derived_message_key(&request.clone(), None, 490_000));
        assert_ne!(key, derived_message_key(&request, None, 490_001));
        assert_ne!(
            key,
            derived_message_key(&request, Some("incarnation/2"), 490_000)
        );
        let mut changes = Vec::new();
        for change in 0..8 {
            let mut changed = request.clone();
            match change {
                0 => changed.from = "person/blair".into(),
                1 => changed.to = "agent/example/other".into(),
                2 => changed.in_reply_to = Some("message/example".into()),
                3 => changed.title = None,
                4 => changed.content = "The merge train is paused.".into(),
                5 => changed.tags = vec!["work".into()],
                6 => {
                    changed.attachments = vec![st3::model::AttachmentInput {
                        blob: format!("blob/{}", "a".repeat(64)),
                        media_type: "image/png".into(),
                        name: None,
                    }]
                }
                _ => changed.idempotency_key = "ignored".into(),
            }
            changes.push(derived_message_key(&changed, None, 490_000));
        }
        // Everything the message carries names it; the key field it fills does not.
        assert!(changes[..7].iter().all(|changed| *changed != key));
        assert_eq!(changes[7], key);

        assert_eq!(shell_word("st3-message:v1:0f"), "st3-message:v1:0f");
        assert_eq!(shell_word("it's mine"), r"'it'\''s mine'");
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
            attachments: Vec::new(),
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
                    attachments: Vec::new(),
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
                    attachments: Vec::new(),
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
            "example/cos/standing/cos",
            "--harness",
            "claude",
            "--model",
            "opus",
            "--as",
            "person/alex",
            "--print-kdl",
        ])
        .unwrap();
        let Command::Agents {
            command: AgentsCommand::Start(args),
        } = cli.command
        else {
            panic!("agents start did not parse");
        };
        let kdl = agent_start_document(&args, None).unwrap();
        let intent = st3::parse_intent(&kdl, "node").unwrap();
        assert!(intent.subjects.contains_key("agent/example/cos/standing/cos"));
        assert!(
            intent.subjects["agent/example/cos/standing/cos"]
                .owner_run
                .is_none()
        );

        let cli = Cli::try_parse_from([
            "st3",
            "agents",
            "stop",
            "example/cos/standing/cos",
            "--as",
            "person/alex",
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
    fn agent_rename_needs_a_nonempty_label_or_clear() {
        let rename = |label: &[&str]| {
            Cli::try_parse_from(
                ["st3", "agents", "rename", "agent/worker"]
                    .into_iter()
                    .chain(label.iter().copied())
                    .chain(["--as", "person/alex"]),
            )
        };
        assert!(rename(&["Garden"]).is_ok());
        assert!(rename(&["--clear"]).is_ok());
        assert!(rename(&[""]).is_err());
        assert!(rename(&[]).is_err());
    }

    fn agent_new_args(arguments: &[&str]) -> AgentNewArgs {
        let cli = Cli::try_parse_from(
            ["st3", "agents", "new"]
                .into_iter()
                .chain(arguments.iter().copied()),
        )
        .unwrap();
        let Command::Agents {
            command: AgentsCommand::New(args),
        } = cli.command
        else {
            panic!("agents new did not parse");
        };
        args
    }

    #[test]
    fn creation_cli_accepts_a_literal_first_message_and_plain_shell_options() {
        let args = agent_new_args(&[
            "worker",
            "--harness",
            "codex",
            "--message",
            "--literal first message",
        ]);
        assert_eq!(args.message.as_deref(), Some("--literal first message"));
        assert_eq!(
            normalize_member_subject(
                "terminal/pty/person/ada/019a0000-0000-7000-8000-000000000001",
                "pty"
            ),
            "pty/person/ada/019a0000-0000-7000-8000-000000000001"
        );
        assert_eq!(
            normalize_member_subject("terminal/agent/test.worker", "pty"),
            "agent/test.worker"
        );
        let source = agent_new_document(&args, "/tmp", false);
        let intent = st3::graph::parse_intent(&source, "test").unwrap();
        assert!(intent.subjects.values().next().unwrap().member.is_some());
        let cli = Cli::try_parse_from([
            "st3",
            "terminals",
            "new",
            "Shell",
            "--host",
            "builder",
            "--cwd",
            "/tmp",
            "--as",
            "person/ada",
        ])
        .unwrap();
        let Command::Terminals {
            command: PtyCommand::New(args),
        } = cli.command
        else {
            panic!()
        };
        assert_eq!(args.name.as_deref(), Some("Shell"));
        assert_eq!(args.host.as_deref(), Some("builder"));
        let cli = Cli::try_parse_from([
            "st3",
            "terminals",
            "end",
            "terminal/pty/person/ada/019a0000-0000-7000-8000-000000000001",
            "--as",
            "person/ada",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Terminals {
                command: PtyCommand::End(_)
            }
        ));
    }

    #[test]
    fn a_new_claude_agent_is_the_fleet_claude_seat() {
        let args = agent_new_args(&[
            "site",
            "--host",
            "builder",
            "--model",
            "claude-opus-5-5",
            "--effort",
            "high",
            "--description",
            "Builds the example site.",
            "--attach",
        ]);
        assert!(args.attach && args.actor.is_none() && args.workspace.is_none());
        let kdl = agent_new_document(&args, "/home/example/st/agents/site", true);
        assert!(kdl.contains(r#"workspace "/home/example/st/agents/site" create=#true"#));
        let intent = st3::parse_intent(&kdl, "laptop").unwrap();
        let seat = &intent.subjects["agent/builder.site"];
        assert!(seat.owner_run.is_none());
        let member = seat.member.as_ref().unwrap();
        assert_eq!(member.host, "builder");
        assert_eq!(member.workspace, "/home/example/st/agents/site");
        assert!(member.workspace_create);
        assert_eq!(member.driver.as_deref(), Some("claude"));
        assert_eq!(member.restart, st3::model::RestartType::Always);
        assert_eq!(member.environment["CLAUDE_CODE_CHILD_SESSION"], "0");
        let st3::model::LaunchSpec::Argv(argv) = &member.launch else {
            panic!("a harness seat launches an argv");
        };
        let joined = argv.join(" ");
        for expected in [
            "--channels plugin:st-channel@st",
            "--model claude-opus-5-5",
            "--effort high",
            "--dangerously-skip-permissions",
            CLAUDE_SEAT_SETTINGS,
        ] {
            assert!(
                joined.contains(expected),
                "{expected} is missing from {joined}"
            );
        }
        let render = seat.desired["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|child| child["name"] == "render")
            .unwrap();
        assert_eq!(render["children"][0]["arguments"], json!([".claude/"]));
        assert_eq!(
            render["children"][1]["arguments"],
            json!([".claude/settings.local.json", CLAUDE_SEAT_SETTINGS])
        );
    }

    #[test]
    fn a_new_codex_agent_is_the_fleet_codex_seat() {
        let args = agent_new_args(&[
            "example/codex",
            "--harness",
            "codex",
            "--model",
            "gpt-example",
            "--effort",
            "medium",
            "--workspace",
            "/srv/example",
            "--print-kdl",
            "--as",
            "person/avery",
        ]);
        assert_eq!(args.actor.as_deref(), Some("person/avery"));
        let kdl = agent_new_document(&args, "/srv/example", false);
        assert!(!kdl.contains("create="));
        assert!(!kdl.contains("host "));
        let intent = st3::parse_intent(&kdl, "laptop").unwrap();
        let member = intent.subjects["agent/example/codex"]
            .member
            .as_ref()
            .unwrap();
        assert_eq!(member.host, "laptop");
        assert!(!member.workspace_create);
        assert!(member.environment.is_empty());
        let st3::model::LaunchSpec::Argv(argv) = &member.launch else {
            panic!("a harness seat launches an argv");
        };
        let joined = argv.join(" ");
        for expected in [
            "--model gpt-example",
            "-c model_reasoning_effort=medium",
            "--dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust",
        ] {
            assert!(
                joined.contains(expected),
                "{expected} is missing from {joined}"
            );
        }
        assert!(!kdl.contains("render") && !kdl.contains(".st3"));
    }

    #[test]
    fn a_new_agent_needs_its_own_identity_inside_a_harness() {
        let command = Cli::try_parse_from(["st3", "agents", "new", "site"])
            .unwrap()
            .command;
        let refused = guard_mutating_cli_actor(&command, Some("agent/seat"), None).unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("needs explicit --as agent/seat")
        );
        let command = Cli::try_parse_from(["st3", "agents", "new", "site", "--as", "agent/seat"])
            .unwrap()
            .command;
        guard_mutating_cli_actor(&command, Some("agent/seat"), None).unwrap();
        assert!(
            Cli::try_parse_from(["st3", "agents", "new", "site", "--attach", "--print-kdl"])
                .is_err()
        );
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
            vec!["st3", "attention", "ls", "--as", "alex"],
            vec!["st3", "devices", "--as", "alex"],
            vec!["st3", "import", "run", "session/demo", "--as", "alex"],
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
            Cli::try_parse_from(["st3", "devices", "--as", "person/alex", "pair", "iPhone"])
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
            "person/alex",
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
    fn subscription_request_decisions_take_a_person_or_agent() {
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
        // Free mode: an agent decides as itself; a bare name is still refused.
        assert!(
            Cli::try_parse_from([
                "st3",
                "missions",
                "cancel-request",
                "request-id",
                "--as",
                "agent/node.triage",
                "--reason",
                "an agent decides too",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "st3",
                "missions",
                "cancel-request",
                "request-id",
                "--as",
                "operator",
                "--reason",
                "a bare name is not an actor",
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
            Cli::try_parse_from(["st3", "attention", "ls", "--as", "person/alex"]).unwrap();
        let Command::Attention {
            command: AttentionCommand::Ls { actor, .. },
        } = list.command
        else {
            panic!("the review list command did not parse");
        };
        assert_eq!(actor.as_deref(), Some("person/alex"));

        let approve = Cli::try_parse_from([
            "st3",
            "attention",
            "approve",
            "mission-run/release/one",
            "--as",
            "person/alex",
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
            Cli::try_parse_from(["st3", "attention", "ls", "--as", "person/alex"]).unwrap();
        let Command::Attention {
            command: AttentionCommand::Ls { actor, .. },
        } = list.command
        else {
            panic!("the attention list command did not parse");
        };
        assert_eq!(actor.as_deref(), Some("person/alex"));

        let request = Cli::try_parse_from([
            "st3",
            "attention",
            "request",
            "--for",
            "person/alex",
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
            "person/alex",
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
            "person/alex",
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
    fn a_native_driver_has_explicit_paths_without_catalogs() {
        let root = tempfile::tempdir().unwrap();
        for subject in [
            "agent/node.worker",
            "agent/example/app-web/standing/app-web",
        ] {
            let (driver_root, agent_dir, identity, runtime_id) =
                prepare_native_driver_in(subject, root.path()).unwrap();
            assert_eq!(agent_dir, driver_root.join("observations"));
            assert!(agent_dir.is_dir());
            assert!(!driver_root.join("catalog").exists());
            assert!(!driver_root.join("catalog.kdl").exists());
            assert!(!agent_dir.join("agent.kdl").exists());
            assert_eq!(identity, subject.strip_prefix("agent/").unwrap());
            assert_eq!(runtime_id, identity);
            assert_eq!(
                prepare_native_driver_in(subject, root.path()).unwrap().1,
                agent_dir
            );
        }
    }

    #[test]
    fn predecessor_layout_survives_resume_without_rewriting_catalogs() {
        let drivers = tempfile::tempdir().unwrap();
        let subject = "agent/example/worker";
        let old = NativePaths::legacy_in(subject, "codex", drivers.path()).unwrap();
        fs::create_dir_all(&old.agent_dir).unwrap();
        fs::write(old.driver_root.join("catalog.kdl"), "predecessor bytes").unwrap();
        fs::write(old.agent_dir.join("agent.kdl"), "predecessor declaration").unwrap();
        fs::create_dir_all(&old.session_dir).unwrap();
        fs::write(old.session_dir.join("binding.json"), "provider binding").unwrap();
        let state = NativeLoopState {
            paths: Some(old.resolved()),
            ..NativeLoopState::default()
        };
        let encoded = serde_json::to_value(&state).unwrap();
        let back: NativeLoopState = serde_json::from_value(encoded.clone()).unwrap();
        let adopted = NativePaths::resumed(subject, "codex", back.paths).unwrap();
        assert_eq!(adopted.resolved(), old.resolved());
        assert_eq!(
            fs::read(adopted.session_dir.join("binding.json")).unwrap(),
            b"provider binding"
        );
        assert_eq!(
            fs::read(old.driver_root.join("catalog.kdl")).unwrap(),
            b"predecessor bytes"
        );
        assert!(!old.state_root().join("observations").exists());
        let mut predecessor = encoded;
        predecessor.as_object_mut().unwrap().remove("paths");
        assert!(
            serde_json::from_value::<NativeLoopState>(predecessor)
                .unwrap()
                .paths
                .is_none()
        );
    }

    #[test]
    fn native_timeline_fences_provider_session_but_claims_runtime_session() {
        let record = st_drivers::harness_timeline::Record {
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
            st_drivers::harness_timeline::Operation {
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
        st_drivers::message::send_to_inbox(
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
        let filename = st_drivers::message::send_to_inbox(
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

    #[tokio::test]
    async fn graph_delivery_gate_closes_on_hold_outage_and_mismatched_subject() {
        use axum::{Json, Router, routing::get};
        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wrong_subject = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler_active = active.clone();
        let handler_wrong = wrong_subject.clone();
        let app = Router::new().route(
            "/v1/delivery/hold",
            get(move || {
                let active = handler_active.clone();
                let wrong = handler_wrong.clone();
                async move {
                    Json(
                        json!({"api_version": "st3.v1", "value": st3::delivery_hold::HoldView {
                            subject: if wrong.load(std::sync::atomic::Ordering::SeqCst) {
                                "agent/eval/other"
                            } else {
                                "agent/eval/gated"
                            }
                            .into(),
                            active: active.load(std::sync::atomic::Ordering::SeqCst),
                            until_unix_ms: None,
                            reason: None,
                            actor: None,
                            claim: None,
                        }}),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Client::new(Endpoint::Http(format!("http://{address}")));
        let gate = st_drivers::session_control::DeliveryGate::default();
        assert!(gate.held());
        refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
            .await
            .unwrap();
        assert!(!gate.held());
        active.store(true, std::sync::atomic::Ordering::SeqCst);
        refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
            .await
            .unwrap();
        assert!(gate.held());
        active.store(false, std::sync::atomic::Ordering::SeqCst);
        wrong_subject.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
                .await
                .is_err()
        );
        assert!(gate.held());
        wrong_subject.store(false, std::sync::atomic::Ordering::SeqCst);
        refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
            .await
            .unwrap();
        assert!(!gate.held());
        let root = tempfile::tempdir().unwrap();
        let request = st3::delivery_hold::HoldRequest {
            subject: "agent/eval/gated".into(),
            actor: "agent/eval/gated".into(),
            held: true,
            until_unix_ms: u64::try_from(current_unix_ms().unwrap()).unwrap() + 60_000,
            reason: "adopted hold".into(),
            idempotency_key: "adopt-test".into(),
            legacy_adoption: true,
        };
        let deadline = request.until_unix_ms;
        let mut paths = NativePaths {
            driver_root: root.path().into(),
            session_dir: root.path().join("sessions"),
            agent_dir: root.path().into(),
            identity: "h.gated".into(),
            runtime_id: "gated".into(),
            delivery_gate: gate.clone(),
            pending_hold_adoption: Some(request),
        };
        assert!(
            refresh_native_delivery_control(&client, "agent/eval/gated", &mut paths)
                .await
                .is_err()
        );
        assert!(gate.held());
        assert_eq!(
            paths.pending_hold_adoption.as_ref().unwrap().until_unix_ms,
            deadline
        );
        paths.pending_hold_adoption.as_mut().unwrap().until_unix_ms = 0;
        refresh_native_delivery_control(&client, "agent/eval/gated", &mut paths)
            .await
            .unwrap();
        assert!(paths.pending_hold_adoption.is_none());
        assert!(!gate.held());
        server.abort();
        assert!(
            refresh_graph_delivery_gate(&client, "agent/eval/gated", &gate)
                .await
                .is_err()
        );
        assert!(gate.held());
    }

    #[test]
    fn a_graph_read_releases_the_native_delivery_fifo() {
        let root = tempfile::tempdir().unwrap();
        let inbox = root.path().join("inbox");
        let archive = root.path().join("archive");
        let old = st_drivers::message::send_to_inbox(
            &inbox,
            "requester",
            Some("Already read"),
            None,
            &["st3-message:message/old".into()],
            "The recipient read this through the graph.",
        )
        .unwrap();
        let next = st_drivers::message::send_to_inbox(
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
        assert_eq!(st_drivers::message::list_dir(&archive).unwrap().len(), 1);
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
        let filename = st_drivers::message::send_to_inbox(
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
    async fn legacy_native_mailbox_replays_delivered_unread_once_and_keeps_read_closed_final() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open_memory("replay").unwrap());
        let seat = "agent/eval.worker";
        for final_status in ["delivered", "read", "closed"] {
            let subject = format!("message/{final_status}");
            store
                .append_claim(&ClaimInput {
                    subject: subject.clone(),
                    kind: "message.sent".into(),
                    actor: Some("person/eval".into()),
                    fields: BTreeMap::from([
                        ("from".into(), json!("person/eval")),
                        ("to".into(), json!(seat)),
                        ("content".into(), json!("Recovered brief")),
                        ("status".into(), json!("sent")),
                    ]),
                    evidence: Vec::new(),
                    expected_subject: None,
                    idempotency_key: None,
                })
                .unwrap();
            for status in ["delivered", "read", "closed"] {
                store
                    .append_claim(&ClaimInput {
                        subject: subject.clone(),
                        kind: format!("message.{status}"),
                        actor: Some(seat.into()),
                        fields: BTreeMap::from([("status".into(), json!(status))]),
                        evidence: Vec::new(),
                        expected_subject: None,
                        idempotency_key: None,
                    })
                    .unwrap();
                if status == final_status {
                    break;
                }
            }
        }
        let (client, server) = serve_test_store(store.clone(), root.path(), "replay").await;
        let inbox = root.path().join("inbox");
        let archive = root.path().join("archive");
        let mut first_filename = None;
        for _ in 0..3 {
            forward_projected_messages(
                &client,
                seat,
                &inbox,
                &archive,
                "app-server",
                NativeDeliveryReceipts::Codex {
                    state_dir: root.path(),
                    identity: "eval.worker",
                    runtime_id: "worker",
                },
            )
            .await
            .unwrap();
            let messages = st_drivers::message::list_dir(&inbox).unwrap();
            let subjects = messages
                .iter()
                .flat_map(|message| &message.tags)
                .filter_map(|tag| tag.strip_prefix("st3-message:"))
                .collect::<Vec<_>>();
            assert_eq!(subjects, ["message/delivered"]);
            let filename = &messages[0].filename;
            if let Some(first) = &first_filename {
                assert_eq!(filename, first, "replay must retain the native handoff identity");
            } else {
                first_filename = Some(filename.clone());
            }
        }
        assert_eq!(store.message("message/delivered").unwrap().unwrap().status, "delivered");
        assert_eq!(store.message("message/read").unwrap().unwrap().status, "read");
        assert_eq!(store.message("message/closed").unwrap().unwrap().status, "closed");
        server.abort();
    }

    #[tokio::test]
    async fn consumed_delivered_unread_mail_posts_no_lifecycle_claim_on_any_poll() {
        use axum::{
            Json, Router,
            extract::Path as AxumPath,
            routing::{get, post},
        };

        // #1085: a seat holding many delivered-unread messages re-posted `delivered` for every
        // one of them on every poll, and each repeat woke every mailbox reader in the daemon.
        let posts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = posts.clone();
        let app = Router::new()
            .route(
                "/v1/messages/page",
                get(|| async {
                    let message = |subject: &str, status: &str, index: u64| {
                        json!({"subject": subject, "from": "agent/sender", "to": "agent/test",
                            "content": "Signal", "status": status, "created_index": index})
                    };
                    Json(json!({"api_version": "st3.v1", "value": {
                        "items": [message("message/delivered", "delivered", 1),
                                  message("message/staged", "staged", 2)],
                        "has_more": false, "next_cursor": null, "limit": 200
                    }}))
                }),
            )
            .route(
                "/v1/messages/{message_id}/claims",
                post(move |AxumPath(message_id): AxumPath<String>| {
                    let recorded = recorded.clone();
                    async move {
                        recorded.lock().unwrap().push(message_id.clone());
                        Json(json!({"api_version": "st3.v1", "value": {
                            "id": "claim/1", "store_index": 1, "batch_id": "batch/1",
                            "subject": message_id, "kind": "message.delivered", "origin": "test",
                            "actor": "agent/test", "body": {}, "predecessors": [],
                            "accepted_at_unix_ms": 1
                        }}))
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
        let mut writer = st_drivers::harness_timeline::Writer::new(root.path(), "claude", "one");
        for subject in ["message/delivered", "message/staged"] {
            let filename = st_drivers::message::send_to_inbox(
                &inbox,
                "agent/sender",
                None,
                None,
                &[format!("st3-message:{subject}")],
                "Signal",
            )
            .unwrap();
            writer
                .append(
                    subject,
                    st_drivers::harness_timeline::Role::User,
                    st_drivers::harness_timeline::EntryType::Content,
                    json!({"text": format!("[st3-delivery:{filename}]\nSignal")}),
                    true,
                )
                .unwrap();
        }
        for _ in 0..5 {
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
        }
        let posts = posts.lock().unwrap().clone();
        // A consumed message still in `staged` records its delivery (here the mock never
        // settles it, so it is posted on each poll); an already-delivered one is never posted.
        assert!(posts.iter().all(|id| id.contains("staged")), "{posts:?}");
        assert_eq!(posts.len(), 5, "{posts:?}");
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

        let projected = st_drivers::message::list_inbox(&inbox).unwrap();
        assert_eq!(projected.len(), 1);
        // The inbox file appends a newline, so the hash must come from the graph content.
        assert_eq!(projected[0].body, "FACT <b>QUARTZ</b>\n");
        let catalog = tempfile::tempdir().unwrap();
        assert_eq!(
            st_drivers::ding::poke_text(catalog.path(), "h", "run-1/wake.right", &projected[0]),
            format!(
                "<smalltalk-message id=\"fact\" from=\"agent/run-1/wake.left\" \
                 to=\"agent/run-1/wake.right\" subject=\"Fact\" sha256=\"{}\" \
                 graph=\"message/fact\">\nFACT &lt;b&gt;QUARTZ&lt;/b&gt;\n</smalltalk-message>",
                st_drivers::ding::st3_body_sha256("FACT <b>QUARTZ</b>")
            )
        );
    }

    /// A renewal race, where the step's claim ended between reading the work and renewing it, is
    /// not a reason to end the driver. Any other API error still is.
    #[tokio::test]
    async fn a_renewal_that_lost_its_claim_is_skipped_rather_than_ending_the_driver() {
        use axum::{Router, http::StatusCode, response::IntoResponse as _, routing::post};

        let app = Router::new()
            .route(
                "/v1/work/renew/step-run/lost",
                post(|| async {
                    (
                        StatusCode::CONFLICT,
                        axum::Json(serde_json::json!({
                            "code": "work-not-claimed",
                            "message": "the step is not claimed"
                        })),
                    )
                        .into_response()
                }),
            )
            .route(
                "/v1/work/renew/step-run/broken",
                post(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({
                            "code": "internal",
                            "message": "the store failed"
                        })),
                    )
                        .into_response()
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::new(Endpoint::Http(format!("http://{address}")));
        let renew = |step: &'static str| {
            let client = &client;
            async move {
                client
                    .post::<_, Value>(
                        &format!("/v1/work/renew/step-run/{step}"),
                        &serde_json::json!({}),
                    )
                    .await
                    .unwrap_err()
            }
        };
        assert!(renewal_lost_its_claim(&renew("lost").await));
        assert!(!renewal_lost_its_claim(&renew("broken").await));
        server.abort();
    }

    #[tokio::test]
    async fn one_message_that_cannot_be_forwarded_does_not_hold_back_the_next() {
        use axum::{Json, Router, routing::get};

        let app = Router::new().route(
            "/v1/messages/page",
            get(|| async {
                Json(serde_json::json!({
                    "api_version": "st3.v1",
                    "value": {
                        "items": [
                            {
                                "subject": "message/missing", "from": "agent/sender",
                                "to": "agent/test", "content": "doc/notes/missing@abc",
                                "status": "staged", "created_index": 1
                            },
                            {
                                "subject": "message/next", "from": "agent/sender",
                                "to": "agent/test", "content": "the next message",
                                "status": "staged", "created_index": 2
                            }
                        ],
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

        let unforwarded = forward_projected_messages(
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
        server.abort();

        assert_eq!(
            unforwarded
                .iter()
                .map(|(message, _)| message.as_str())
                .collect::<Vec<_>>(),
            ["message/missing"]
        );
        let projected = st_drivers::message::list_inbox(&inbox).unwrap();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].body, "the next message\n");
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
                workspace: root.to_owned(),
                no_gate_check: false,
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
    #[tokio::test]
    async fn committed_observations_survive_daemon_outage_lost_ack_and_driver_reexec() {
        use axum::{Json, Router, http::StatusCode, response::IntoResponse as _, routing::post};
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        let root = tempfile::tempdir().unwrap();
        st_drivers::harness_events::enable(root.path(), "runtime-a").unwrap();
        let seq =
            st_drivers::harness_state::claim(root.path(), "example/seat", "claude", "provider-a")
                .unwrap();
        let mut writer = st_drivers::harness_state::Writer::new(
            root.path(),
            "example/seat",
            "claude",
            Some("pty".into()),
        )
        .with_ownership("provider-a", seq);
        writer
            .observe(st_drivers::harness_state::Observation::new(
                st_drivers::harness_state::Activity::Active,
                st_drivers::harness_state::BlockedOn::None,
                st_drivers::harness_state::InputBuffer::Unknown,
            ))
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let client = Client::new(st3::client::Endpoint::Http(format!("http://{address}")));
        let mut observations = NativeObservations::start(root.path(), "runtime-a").unwrap();
        let mut ready = false;
        assert!(
            observations
                .drain(&client, "agent/example/seat", "claude", &mut ready)
                .await
                .is_err()
        );
        assert_eq!(
            st_drivers::harness_events::pending(root.path(), 100)
                .unwrap()
                .len(),
            1
        ); // Only the non-observation claim placeholder was acknowledged locally.
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let store = Arc::new(Store::open_memory("amber").unwrap());
        let refuse_once = Arc::new(AtomicBool::new(true));
        let app = Router::new().route(
            "/v1/harness-events",
            post({
                let captured = captured.clone();
                let store = store.clone();
                move |Json(request): Json<st3::harness_events::Publication>| {
                    let captured = captured.clone();
                    let store = store.clone();
                    let refuse_once = refuse_once.clone();
                    async move {
                        captured
                            .lock()
                            .unwrap()
                            .push(serde_json::to_value(&request).unwrap());
                        let record = store.append_claim(&request.claim).unwrap();
                        if refuse_once.swap(false, Ordering::SeqCst) {
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({"error":"acknowledgement lost"})),
                            )
                                .into_response()
                        } else {
                            Json(json!({"api_version":"st3.v1","value":record})).into_response()
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(
            observations
                .drain(&client, "agent/example/seat", "claude", &mut ready)
                .await
                .is_err()
        );
        assert_eq!(
            st_drivers::harness_events::pending(root.path(), 100)
                .unwrap()
                .len(),
            1
        );
        drop(observations); // A replacement image reconstructs all pending work from the outbox.
        let mut replacement = NativeObservations::start(root.path(), "runtime-a").unwrap();
        replacement
            .drain(&client, "agent/example/seat", "claude", &mut ready)
            .await
            .unwrap();
        assert!(ready);
        assert!(
            st_drivers::harness_events::pending(root.path(), 100)
                .unwrap()
                .is_empty()
        );
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0], captured[1]);
        assert_eq!(store.local_observations_tail(100).unwrap().len(), 1);
        server.abort();
    }
    #[tokio::test]
    async fn reading_the_outbox_does_not_wake_an_idle_driver() {
        let root = tempfile::tempdir().unwrap();
        st_drivers::harness_events::enable(root.path(), "runtime").unwrap();
        let mut observations = NativeObservations::start(root.path(), "runtime").unwrap();
        observations.recv().await.unwrap();
        assert!(
            st_drivers::harness_events::pending(root.path(), 100)
                .unwrap()
                .is_empty()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), observations.recv())
                .await
                .is_err()
        );
        st_drivers::harness_events::write_channel_todo(root.path(), "runtime", &json!({
            "harness":"omp", "session_id":"native", "incarnation_id":"runtime",
            "observed_at":"2026-10-03T15:00:00Z", "source_op":"hydrate", "phases":[],
            "totals":{"pending":0,"in_progress":0,"completed":0,"blocked":0}, "truncated":false,
        })).unwrap();
        tokio::time::timeout(Duration::from_secs(1), observations.recv()).await.unwrap().unwrap();
    }
}
