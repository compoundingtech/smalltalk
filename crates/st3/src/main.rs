use st3::cli_output;
use st3::{cli_print as print, cli_println as println};

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
#[cfg(test)]
use st3::api::serve_unix;
use st3::api::{AppState, fabric_router, router};
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
    MissionRunReportRequest, MissionRunReportView, MissionRunView, MissionSpec, MissionState, OperationalRepairApplyRequest,
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

use clap_complete::engine::ArgValueCompleter;
use completion::{Complete, Entity, WorkFilter};

mod cli_help;
mod setup;
mod onboarding;
mod change_cli;
mod directive_note;
mod completion;
mod doctor_cli;
#[cfg(test)]
mod follow_tests;
mod presentation;

// Command families: each file holds one command's implementation; main.rs dispatches to it.
mod agents_cli;
mod attention_cli;
mod conversations_cli;
mod driver_cli;
mod fleet_cli;
mod launch_cli;
mod replication_cli;
mod terminals_cli;
mod uninstall_cli;
mod work_cli;
use agents_cli::run_agents;
use attention_cli::run_attention;
use conversations_cli::{run_message, upload_attachments};
use driver_cli::{run_driver, run_driver_hook};
use fleet_cli::run_fleet;
use launch_cli::{run_launch, run_mission_view};
use replication_cli::run_replication;
use terminals_cli::run_pty;
use uninstall_cli::run_uninstall;
use work_cli::{run_wait, run_work};

use presentation::{
    OutputStyle, follow_snapshot, glance, mission_run_signature, relative_time,
    render_attention_show, render_generation, render_generations, render_host_facts,
    render_human_value, render_mission_run_page, render_revision_proposal, render_step_run,
    shell_argument,
};

#[derive(Parser)]
#[command(
    name = "st",
    bin_name = "st",
    version = st_drivers::version::display_version(),
    about = "Coordinate durable agent work across machines without losing operational truth"
)]
struct Cli {
    #[arg(long, global = true, hide = true)]
    read_contract_inventory_json: bool,
    #[arg(long, global = true)]
    endpoint: Option<String>,
    /// Run a second, fully separate st on this machine: its own state, config, sockets, node
    /// name, service names and harness accounts, all under ~/.st-instance/NAME. Same as the
    /// ST_INSTANCE environment variable.
    #[arg(long, global = true, env = "ST_INSTANCE", value_name = "NAME")]
    instance: Option<String>,
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
    /// Configure this machine and start st, with flags for unattended setup.
    Setup(crate::setup::SetupArgs),
    /// Open the terminal interface: spaces, conversations, Home and agent terminals.
    #[command(version = st_drivers::version::display_version())]
    Ui(stui::Args),
    /// Inspect Git-backed changes and their separate ref/index/evidence freshness.
    Changes(change_cli::ChangesArgs),
    /// Validate and publish KDL files containing seats, missions and schedules.
    /// Use --dry-run to preview, or --set with source flags to publish a complete owned set.
    /// Gate checks run mission and step scripts; loop until scripts run after each round.
    Apply(ApplyArgs),
    /// Inspect owned sets and their source publication receipts.
    Sets {
        #[command(subcommand)]
        command: OwnedSetsCommand,
    },
    /// Start the HTTP API, readers, peers, and reconciler.
    Up(UpArgs),
    /// Understand what needs action now.
    Now(NowArgs),
    /// Read current person notes; notes inform but never assign work or grant authority.
    Note(directive_note::NoteArgs),
    /// Show token spend over a period, with the largest spenders first.
    Usage(UsageArgs),
    /// List every objective and budget with its target, current value and what is left, and
    /// explain one in plain words.
    Slo(SloArgs),
    /// Inspect and control missions.
    ///
    /// A mission records an authorized goal, its plan, owners, verification and result.
    /// Use work start for a small independent job; use a finite mission for dependencies,
    /// distinct owners, gates or review. st skill explains the agent workflow.
    Missions {
        #[command(subcommand)]
        command: MissionViewCommand,
    },
    /// Create and review a durable planner-backed launch.
    Launch {
        #[command(subcommand)]
        command: LaunchCommand,
    },
    /// Show and answer alerts: asks, gates and approvals that wait on a person.
    ///
    /// Each alert belongs to the conversation of the agent behind it. Updates that ask nothing
    /// are listed after the alerts and are not counted. `st attention` is the older name.
    #[command(
        name = "alerts",
        alias = "attention",
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
    /// The clients connected to this member now and those seen in the last few minutes:
    /// stui, the phone and st itself, with their builds as they report them.
    Clients,
    /// Claim and update durable mission work.
    ///
    /// Claim existing work first. work start records a small independent authorized job;
    /// missions record a plan with dependencies and review. Complete work with evidence,
    /// then turn review feedback into the next authorized iteration. See st skill.
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
    /// Save or restore the signed claim log.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
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
    /// Import, inspect, and verify signed gate observations stored in git notes.
    Evidence(st3::evidence::cli::EvidenceArgs),
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
    /// Run any CLI with credentials no seat can read, through the sekrets gateway.
    Sekrets(st3::sekrets::cli::SekretsArgs),
    /// Run an agent's tools in its tile: its own Unix user, namespaces and cgroup. Root only.
    Tiles(st3::tiles::cli::TilesArgs),
    /// Discover native harness sessions and move one under durable st ownership.
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    /// Print one shell's completion stub. On each TAB the stub asks this st executable for
    /// candidates, including live terminals, agents, missions, and other entities from the daemon.
    Completions(CompletionsArgs),
    /// Answer one built-in gate check, as built-in gates run it: exit 0 to pass, 1 for not yet,
    /// and 3 when the check cannot answer.
    Gate {
        #[command(subcommand)]
        command: GateCommand,
    },
    /// Print the st agent skill bundled in this binary, or install it for each harness.
    Skill(SkillArgs),
    /// Allow or revoke a local exact-build harness admission exception.
    Admission {
        #[command(subcommand)]
        command: AdmissionCommand,
    },
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
    /// Add or remove Fabric on this member without rejoining.
    Transports {
        #[command(subcommand)]
        command: FleetTransportsCommand,
    },
    /// Show this node, the fleet's members, and open invites.
    Status,
    /// Wait for this node's first sync to end: until it holds the same claims as the peer it
    /// synced from and has admitted them all.
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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

fn services_installed() -> bool {
    st3::service::status()
        .map(|report| report.services.iter().any(|service| service.installed))
        .unwrap_or(false)
}

#[derive(Args)]
struct FleetRemoveArgs {
    /// The member (or config peer) to remove.
    name: String,
    #[arg(long)]
    reason: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    st3::config::FleetFile::remove(&config.state_dir)?;
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

/// What an instance's uninstall removes from its directory: every entry except the one that holds
/// the work root. The work root can lie inside the instance but holds the person's work, so
/// everything else goes and it stays.
fn instance_removals(root: &Path, work: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| !work.starts_with(path))
                .collect()
        })
        .unwrap_or_default()
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

#[derive(Subcommand)]
enum FleetTransportsCommand {
    /// Expose Fabric, then announce it. Requires a listening member with a port.
    Add {
        #[arg(value_parser = ["fabric"])]
        transport: String,
        /// Use this protocol instead of the saved protocol or st3/fleet/FLEET_ID.
        #[arg(long)]
        fabric_protocol: Option<String>,
        /// Leave the replication worker restart to the operator.
        #[arg(long)]
        no_service: bool,
    },
    /// Withdraw Fabric's endpoint, then unexpose it. Keep at least one usable transport.
    Remove {
        #[arg(value_parser = ["fabric"])]
        transport: String,
        /// Leave the replication worker restart to the operator.
        #[arg(long)]
        no_service: bool,
    },
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
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
    /// Legacy: use `st apply FILE`; publish authored KDL after checking exec gates.
    ///
    /// Goals, constraints and named documents encode every known rule and decision.
    /// `depends-on` orders steps; `missions start --after` orders runs without reports.
    /// A final step assigned to the author, depending on the last real step, reaches the
    /// author once when work is done. Review gates mark decisions only a person can make.
    /// Preview with --dry-run before publication; it runs no exec gates unless --check is set.
    /// Actual publication checks mission and step scripts and refuses broken answers, allowing
    /// valid "not yet" answers. Loop until scripts are not checked; they run after each round.
    /// See st skill for goals, evidence, review and feedback loops.
    Publish(MissionPublishArgs),
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases
    /// Check mission and step exec gates once and report pass (exit 0), not yet (exit 1),
    /// broken (anything else), or unchecked. Loop until scripts are not checked;
    /// they run after each round.
    Check(MissionCheckArgs),
    /// Start one run from the current ready mission revision.
    ///
    /// Goals, constraints and named documents encode every known rule and decision.
    /// `depends-on` orders steps; `--after` orders runs without reports.
    /// A final step assigned to the author, depending on the last real step, reaches the
    /// author once when work is done. Review gates mark decisions only a person can make.
    /// Publication and preview do not start a run. Claim its ready steps, verify the result
    /// and record evidence before review. Recording work does not widen its authorization.
    Start(MissionRunStartArgs),
    /// Cancel one exact running mission and stop its owned work and runtimes.
    Cancel(MissionCancelArgs),
    /// Change who a running run tells when it fails, is cancelled or stalls, or clear it.
    ///
    /// Takes effect at the run's next evaluation. A stall is measured from the run's last sign
    /// of life, so turning this on for a run that is already quiet reports it at most once.
    ReportTo(MissionReportToArgs),
    /// Set a finished run's outcome to completed, failed, or cancelled, with a reason.
    Outcome(MissionOutcomeArgs),
    /// Retire a mission so it leaves the lists and cannot start; publishing it again brings it back.
    Retire(MissionRetireArgs),
    /// Show one seat's current claim and its queued mission runs in order; same as `st agents queue AGENT`.
    Queued {
        /// Exact seat subject or its identity without the `agent/` prefix.
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
        agent: String,
    },
    /// Show one subscription's automatic mission request queue.
    Requests {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Subscription)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionOrRun)))]
    mission_or_run: String,
    /// This read needs no actor. Accepted for scripts and ignored with one notice.
    #[arg(long = "as", value_name = "ACTOR")]
    ignored_actor: Option<String>,
    /// Follow until finished or stopped; retry timeouts and wait up to 5min for an unreachable daemon.
    #[arg(long)]
    follow: bool,
    /// Continue the human root tree after the preceding page's cursor.
    #[arg(long, conflicts_with = "follow")]
    cursor: Option<String>,
    /// Maximum root runs to include in one human tree page.
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

#[derive(Args)]
struct MissionPublishArgs {
    /// KDL file to publish; use `-` to read standard input.
    file: PathBuf,
    /// Print the resolved publication preview without applying or running exec gates.
    /// Add --check to run mission and step gates during the preview; until scripts run after rounds.
    #[arg(long, visible_alias = "preview")]
    dry_run: bool,
    /// Preview against this exact store index.
    #[arg(long, visible_alias = "at")]
    at_index: Option<u64>,
    /// Complete person or agent subject authoring the publication.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// The workspace a run would use, where publish checks mission and step exec gates.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Publish without checking mission and step exec gates for broken commands.
    #[arg(long)]
    no_gate_check: bool,
    /// Check mission and step exec gates during a dry run. Until scripts run after each round.
    #[arg(long, visible_alias = "check-gates", conflicts_with = "no_gate_check")]
    check: bool,
    /// Mission input values for exec gate checks.
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Mission)))]
    mission: String,
    /// Start exactly this published revision, as printed by `st apply`. A revision
    /// published on another host is awaited briefly while it replicates here.
    #[arg(long)]
    revision: Option<String>,
    /// A bare ID is placed under the mission: for release/demo, `--id 1` starts
    /// `mission-run/release/demo/1`. IDs containing a path, such as `--id release/demo/1`,
    /// are used as given. Defaults to MISSION/UUIDv7.
    #[arg(long)]
    id: Option<String>,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
    /// Start no work until this mission run completes; fail if it fails or is cancelled.
    #[arg(long, value_name = "RUN")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: true })))]
    after: Option<String>,
    /// Tell this agent when the run fails, is cancelled or stalls, replacing the mission's own
    /// `report-to`. A run that sits 30 minutes without progress counts as stalled unless the
    /// mission sets `stalled-after`.
    #[arg(long, value_name = "AGENT")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    report_to: Option<String>,
    /// Also tell the reporting agent when the run completes.
    #[arg(long, requires = "report_to")]
    report_completed: bool,
    /// Follow until finished or stopped; retry timeouts and wait up to 5min for an unreachable daemon.
    #[arg(long)]
    follow: bool,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// Print the exact mission-run KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct MissionCancelArgs {
    /// Exact mission-run subject to cancel.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: true })))]
    mission_run: String,
    /// Why the run is no longer wanted.
    #[arg(long)]
    reason: String,
    /// Person or authorized agent carried over the trusted local Unix boundary.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
}

#[derive(Args)]
#[command(group = clap::ArgGroup::new("reporter").required(true).args(["agent", "clear"]))]
struct MissionReportToArgs {
    /// Exact mission-run subject of a running run.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: true })))]
    mission_run: String,
    /// The agent to tell, replacing whoever the run reports to now. A person is reached
    /// through their own agent.
    #[arg(long, value_name = "AGENT")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    agent: Option<String>,
    /// How long the run may go without progress before it counts as stalled, such as `1h`.
    /// Defaults to the mission's `stalled-after`, else 30 minutes.
    #[arg(long, value_name = "DURATION", requires = "agent", conflicts_with = "clear")]
    #[arg(value_parser = parse_stalled_after)]
    stalled_after: Option<u64>,
    /// Report stalls (true or false). Defaults to the mission's policy, else true.
    #[arg(long, requires = "agent", conflicts_with = "clear", action = clap::ArgAction::Set)]
    report_stalls: Option<bool>,
    /// Also tell the agent when the run completes.
    #[arg(long, requires = "agent", conflicts_with = "clear")]
    report_completed: bool,
    /// Report this run to nobody, whatever its mission or start named.
    #[arg(long)]
    clear: bool,
    /// A person, or the agent that requested the run.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
}

fn parse_stalled_after(value: &str) -> Result<u64, String> {
    st3::graph::parse_duration(value, true).map_err(|error| error.to_string())
}

#[derive(Args)]
struct MissionOutcomeArgs {
    /// Exact mission-run subject of a finished run.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: false })))]
    mission_run: String,
    /// The outcome the run should show.
    #[arg(value_parser = ["completed", "failed", "cancelled"])]
    status: String,
    /// Why the run has this outcome, for example that its work shipped after a gate failed.
    #[arg(long)]
    reason: String,
    /// A person, or an agent that requested the run or may revise its mission.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
}

#[derive(Args)]
struct MissionRetireArgs {
    /// Mission to retire, such as `mission/fleet/demo/deploy`.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Mission)))]
    mission: String,
    /// A person, or an agent that may publish the mission.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
}

#[derive(Args)]
struct PlanningPreviewArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    #[arg(long)]
    variant: Option<String>,
}

#[derive(Args)]
struct PlanningSubmitArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    #[arg(long, default_value = "default")]
    variant: String,
    #[arg(long)]
    markdown: PathBuf,
    #[arg(long)]
    kdl: PathBuf,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: String,
}

#[derive(Args)]
struct PlanningCompareArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    left: String,
    right: String,
}

#[derive(Args)]
struct PlanningProposeArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    variant: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long)]
    reason: String,
}

#[derive(Args)]
struct PlanningReviseArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    feedback: PathBuf,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
    /// Print the feedback KDL without storing the feedback or publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct PlanningApproveArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    preview_hash: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args)]
struct LaunchApproveAndStartArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    preview_hash: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args)]
struct LaunchRunArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args)]
struct LaunchQuestionArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    question: String,
    #[arg(long = "type", value_parser = parse_launch_decision_type)]
    decision_type: LaunchDecisionType,
    /// Structured JSON option: {"id":"stable-id","label":"Label","description":"optional"}.
    #[arg(long = "option", value_parser = parse_launch_decision_option)]
    options: Vec<LaunchDecisionOption>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: String,
}

#[derive(Args)]
struct LaunchAnswerArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    decision: String,
    /// Structured JSON response such as {"type":"multiple-choice","value":["a","b"]}.
    #[arg(value_parser = parse_launch_decision_response)]
    response: LaunchDecisionResponse,
    #[arg(long)]
    explanation: Option<String>,
    #[arg(long, default_value_t = 1)]
    expected_revision: u32,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args)]
struct PlanningCancelArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Launch)))]
    session: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        /// Match the exact owner subject before pagination (for example agent/example/worker).
        #[arg(long, add = ArgValueCompleter::new(Complete(Entity::Actor)))]
        owner: Option<String>,
        /// Match the exact projected runtime state, such as running.
        #[arg(long)]
        state: Option<String>,
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
    /// Fabric keeps the exposure in its own configuration and forwards each tunnel to the st
    /// daemon's terminal socket, which serves it on its own thread, so a busy daemon does not
    /// delay an attach. While the daemon is stopped or restarting, as during every deploy, it
    /// refuses new tunnels and its live remote attaches drop; `st terminals attach` reconnects
    /// once it is back. `--exec` instead has Fabric run `st terminals serve-fabric` for each
    /// tunnel, which works without the daemon at the cost of a process and pipes per attach.
    /// Each peer also needs a grant for the printed protocol in this machine's Fabric
    /// `peers.toml`.
    ExposeFabric(PtyExposeFabricArgs),
    /// Serve one Fabric tunnel to a PTY session on stdin and stdout, as `expose-fabric --exec`
    /// has Fabric run it.
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Host)))]
    host: Option<String>,
    /// Absolute directory on the selected host. Locally defaults to the caller's directory;
    /// remotely defaults to the daemon's directory.
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtySubjectArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Terminal)))]
    subject: String,
}

#[derive(Args)]
struct PtyScreenArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Terminal)))]
    subject: String,
    /// Use this concrete person instead of the person configured for trusted local commands.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtyStreamArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Terminal)))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
    /// The stream capability from `terminals attach-info`; can be set through the environment.
    #[arg(long, env = "ST3_TERMINAL_CAPABILITY", allow_hyphen_values = true)]
    capability: String,
    #[arg(long)]
    incarnation: Option<String>,
    /// Stop after this many screens instead of following until the stream ends.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    count: Option<u64>,
}

#[derive(Args)]
struct PtyClientInputArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Terminal)))]
    subject: String,
    value: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtyAttachArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Terminal)))]
    subject: String,
    /// Allow an attachment from inside another PTY session.
    #[arg(long)]
    force: bool,
    /// The person attaching to a terminal another fleet host owns; defaults to `person` in the st
    /// config. A terminal on this host needs no person.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    person: Option<String>,
}

#[derive(Args)]
struct PtyExposeFabricArgs {
    /// Have Fabric run `st terminals serve-fabric` for each tunnel instead of forwarding it to the
    /// daemon's terminal socket, so attaches keep working while the daemon is down.
    #[arg(long)]
    exec: bool,
    /// With `--exec`, the st executable Fabric runs; defaults to this one. Name a stable path,
    /// since Fabric keeps it after this build is replaced.
    #[arg(long, requires = "exec")]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Terminal)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Terminal)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: false })))]
    owner_run: Option<String>,
    #[arg(long, default_value_t = 100)]
    limit: usize,
    #[arg(long)]
    after_index: Option<u64>,
    /// Follow from the last index; retry timeouts and wait up to 5min for an unreachable daemon.
    #[arg(short = 'f', long)]
    follow: bool,
}

#[derive(Args)]
struct WaitArgs {
    subject: String,
    /// Interrupt the wait for this exact agent's messages or newly ready work.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Person)))]
    #[arg(long = "as", value_parser = parse_person_subject)]
    person: Option<String>,
    #[arg(long)]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: false })))]
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
    /// Count coordination messages only, without token usage or message bodies (JSON output).
    #[arg(long)]
    messages_only: bool,
    /// Length of the period ending now.
    #[arg(long, default_value_t = 24)]
    hours: u64,
    /// Show only this grouping; the default shows them all.
    #[arg(long, value_enum)]
    by: Option<UsageBy>,
}

#[derive(Args)]
struct SloArgs {
    #[command(subcommand)]
    command: Option<SloCommand>,
}

#[derive(Subcommand)]
enum SloCommand {
    /// List every objective and budget: target, current value, window and what is left.
    Ls,
    /// Explain one objective or budget in plain words.
    Explain {
        /// An id from `st slo ls`, or the start of one.
        id: String,
    },
    /// Chart what is left of one objective or budget over a period, from the daemon's local
    /// history (a reading a minute, kept seven days).
    Chart {
        /// An id from `st slo ls`, or the start of one.
        id: String,
        /// The period ending now: a number then m, h or d, such as 90m, 24h or 7d.
        #[arg(long, default_value = "24h")]
        over: String,
        /// Columns of chart; the default fits the terminal.
        #[arg(long)]
        width: Option<usize>,
    },
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

/// Observation-only device grant requested by `st devices pair --read-only`.
const READ_ONLY_PAIRING_SCOPES: &[&str] = &["read.projections", "read.glasses", "terminal.read"];

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
        /// Delegate only observation: projections, glasses frames, and terminal output.
        #[arg(long, conflicts_with = "full_control")]
        read_only: bool,
    },
    /// Complete a single-use pairing on this device; read the code privately from stdin.
    Complete {
        /// The member gateway's HTTP or HTTPS origin.
        member_url: String,
        pairing_id: String,
        /// Software key algorithm, also used when importing a key.
        #[arg(long, value_parser = ["ed25519", "p256"], default_value = "p256")]
        algorithm: String,
        /// Import a private mode-0600 DER PKCS#8 file; otherwise generate a key.
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// Credential and private-key profile, shared with stui by default.
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Allow a public HTTP address only when it is already an encrypted path.
        #[arg(long)]
        allow_public_http: bool,
        /// Person-root fingerprint obtained separately from the trusted pairing machine.
        #[arg(long, conflicts_with = "unpinned")]
        fingerprint: Option<String>,
        /// Explicitly bypass person-root identity verification (unsafe against active attackers).
        #[arg(long)]
        unpinned: bool,
    },
    /// Revoke one paired device.
    Revoke {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Device)))]
        device: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Args)]
struct DevicesArgs {
    /// Concrete human authority carried over the trusted local Unix boundary.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Person)))]
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
    /// Reply to a custom source under its current revision and waiting episode.
    Reply {
        subject: String,
        #[arg(long)]
        registration: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        episode: String,
        #[arg(long)]
        fields_file: PathBuf,
        #[arg(long="as",value_parser=parse_actor_subject)]
        actor: String,
        #[arg(long)]
        idempotency_key: String,
    },
    /// Read the revision of complete raw custom inputs for a derived-state basis.
    Basis {
        subject: String,
        #[arg(long = "kind", required = true)]
        kinds: Vec<String>,
    },

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
    /// Inspect local source-build tools instead of daemon health; does not compile or contact GitHub.
    #[arg(long, conflicts_with = "performance")]
    developer: bool,
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
    /// Install channel assets and approval policy; activate only in st seats.
    Install {
        /// Install user assets only; seats use development admission without machine policy.
        #[arg(long)]
        no_policy: bool,
    },
    /// Verify user assets and report approved or development channel admission.
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
enum BackupCommand {
    /// Save one live snapshot to a new file. --database exports an offline copy instead.
    Create {
        file: PathBuf,
        #[arg(long)]
        database: Option<PathBuf>,
    },
    /// Restore offline into an empty database, using a fresh writer identity.
    Restore {
        file: PathBuf,
        #[arg(long)]
        database: Option<PathBuf>,
    },
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
        #[arg(long = "as")]
        actor: Option<String>,
    },
}

#[derive(Subcommand)]
enum BlobCommand {
    /// Keep one image on this member and print its reference. Name it in `conversations send
    /// --attach`, or pass the file there directly. Without `--media-type` the file is prepared as
    /// `--attach` prepares it: an SVG or BMP becomes a PNG, and an image over 10 MiB is scaled.
    Put {
        file: PathBuf,
        /// Upload the bytes exactly, as this type (PNG, JPEG, GIF or WebP); by default the file's own
        /// bytes decide.
        #[arg(long)]
        media_type: Option<String>,
        /// Who uploads; defaults to ST_AGENT, then the configured person.
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
        #[arg(long = "as")]
        actor: Option<String>,
    },
}

#[derive(Subcommand)]
#[command(after_help = "A seat keeps one handoff, doc/agents/IDENTITY/handoff, versioned like any document. \
st restarts a seat for an account or model change, a declaration change, or an operator's or agent's \
request; every session that starts after a restart is given the handoff and the live state of the \
seat's work as its first message. Write what the graph does not hold: what you were in the middle of, \
the branches and pull requests you own, what you wait on, what to do next, and anything running \
that you started. Same-session resume (claude --resume, codex resume) stays preferred where it works.\n\n\
Before a planned restart st sends the seat a \"Prepare for restart\" message with a deadline. Writing the \
handoff acknowledges it; `ready` acknowledges a handoff that is already current. The restart goes ahead \
at the acknowledgement or the deadline, whichever comes first.")]
enum HandoffCommand {
    /// Replace this seat's handoff with FILE, or with standard input when FILE is `-` or absent.
    Write {
        file: Option<PathBuf>,
        /// The seat whose handoff to write; only that seat may write it.
        #[arg(long = "as", value_parser = parse_actor_subject)]
        actor: Option<String>,
    },
    /// Say this seat's handoff is current, so a restart that is waiting for it can go ahead.
    Ready {
        /// The seat that is ready; a readiness claim counts only when it names that seat as its actor.
        #[arg(long = "as", value_parser = parse_actor_subject)]
        actor: Option<String>,
    },
    /// Print a seat's latest handoff (this seat's by default).
    Show {
        /// The seat to read, such as agent/garden/worker.
        #[arg(value_parser = parse_actor_subject)]
        agent: Option<String>,
    },
}

#[derive(Subcommand)]
enum DocCommand {
    /// Store a regular file as one immutable named document version.
    Put {
        file: PathBuf,
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
        #[arg(long = "as")]
        name: String,
    },
    /// Read a document by name (the newest version) or by immutable name-and-hash reference.
    /// Printed to a terminal, markdown is rendered; a pipe, --output and --raw give the bytes.
    Get {
        reference: String,
        #[arg(long)]
        output: Option<PathBuf>,
        /// Print the document's bytes as stored, even on a terminal.
        #[arg(long)]
        raw: bool,
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    Show {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::NativeSession { importable_only: false })))]
        session: String,
    },
    /// Stop an exactly identified running harness and resume it in a durable st mission.
    Run {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::NativeSession { importable_only: true })))]
        session: String,
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
        subject: String,
        /// Include a historical agent that is absent from the operational default.
        #[arg(long)]
        all: bool,
    },
    /// Print one seat's declared workspace directory on its owning host, even when stopped.
    Workspace { subject: String },
    /// Write or read the handoff a seat leaves for the session that follows it.
    Handoff {
        #[command(subcommand)]
        command: HandoffCommand,
    },
    /// Declare one new agent seat, start its harness, and wait until it is ready.
    ///
    /// The declaration is the one a person writes by hand, with the harness defaults of the
    /// fleet's Claude and Codex seats; `--print-kdl` shows it without applying it. With
    /// `--attach`, this terminal attaches once the harness is ready, from any fleet host.
    New(AgentNewArgs),
    /// List repositories already used by a host's agents, from replicated graph evidence.
    Repos {
        /// Host to suggest repositories for; defaults to the connected member.
        #[arg(long)]
        host: Option<String>,
    },
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
    /// Legacy: use `st apply FILE`; preview and apply authored KDL.
    Apply(AgentApplyArgs),
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases
    /// Start a durable seat, patching only explicitly supplied declaration fields. A stopped
    /// mission seat starts again on its run's own declaration.
    Start(AgentStartArgs),
    /// Stop one exact durable seat and every process it started.
    ///
    /// The seat's builds and tests end with it, even those that left its harness's process tree.
    /// A host without a systemd user manager ends only the harness's process tree and process
    /// groups; docs/st3/priority.md says what else keeps running there.
    Stop(AgentStopArgs),
    /// Permanently retire an agent identity; later starts and first sends are refused.
    Retire(AgentRetireArgs),
    /// Restart a top-level or mission seat, preserving its declaration; wait for a new incarnation.
    Restart(AgentRestartArgs),
    /// Acknowledge an interrupted seed attempt and its warning; never gate launch or rearm seed.
    AcknowledgeSeed(AgentAcknowledgeSeedArgs),
    /// Retry a published owned-seat cutover with fresh desired and incarnation fences.
    Rollout(AgentRolloutArgs),
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
struct AgentAcknowledgeSeedArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
    subject: String,
    #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
    reason: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct AgentHoldArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
    subject: String,
    /// Hold new native handoffs for this duration, such as 15m.
    #[arg(long = "for", conflicts_with = "release")]
    duration: Option<String>,
    #[arg(long)]
    release: bool,
    #[arg(long)]
    reason: Option<String>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct AgentRenameArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
    agent: String,
    /// Queued mission run to move.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: true })))]
    run: String,
    /// Put the run first in the seat's queue.
    #[arg(long)]
    top: bool,
    /// Put the run last in the seat's queue.
    #[arg(long)]
    bottom: bool,
    /// Put the run directly before another queued run.
    #[arg(long, value_name = "RUN")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: true })))]
    before: Option<String>,
    /// Put the run directly after another queued run.
    #[arg(long, value_name = "RUN")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: true })))]
    after: Option<String>,
    /// Why the order changed; recorded with the move.
    #[arg(long)]
    reason: Option<String>,
    /// Person or agent making the move; defaults to `person` in the st config. Any agent may move
    /// any seat's queue, its own included.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Lane)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Lane)))]
    lane: String,
    /// The entry subject, or the part after the lane's entry prefix, such as a pull request number.
    entry: String,
    /// Why; recorded with the change.
    #[arg(long)]
    reason: Option<String>,
    /// Person or agent making the change. A harness acts as its own seat (`ST_AGENT`); otherwise
    /// this defaults to `person` in the st config.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Lane)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_queue_move_actor)]
    actor: Option<String>,
}

#[derive(Args)]
struct ApplyArgs {
    /// Input KDL files; '-' reads stdin once. All files are previewed and applied together.
    files: Vec<PathBuf>,
    /// Publish complete owned membership; requires all source flags and --expect-set.
    #[arg(long, requires_all = ["repository", "source_ref", "sha", "source_sequence", "expect_set"])]
    set: Option<String>,
    #[arg(long, requires = "set")]
    repository: Option<String>,
    #[arg(long = "ref", requires = "set")]
    source_ref: Option<String>,
    #[arg(long, requires = "set")]
    sha: Option<String>,
    #[arg(long, requires = "set")]
    source_sequence: Option<u64>,
    /// 'absent' for initial creation, otherwise the previous selected set revision.
    #[arg(long, requires = "set")]
    expect_set: Option<String>,
    /// Print the resolved preview without publishing or running exec gates (unless --check).
    #[arg(long, visible_alias = "preview")]
    dry_run: bool,
    /// Check mission and step exec gates during a dry run, as `st missions check` does.
    /// Loop until scripts are not checked; they run after each round.
    #[arg(long, visible_alias = "check-gates", conflicts_with = "no_gate_check")]
    check: bool,
    /// The workspace a run would use, for exec gate checks.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// Mission input values for exec gate checks.
    #[arg(long = "input", value_parser = parse_input)]
    inputs: Vec<(String, String)>,
    /// Publish without checking mission and step exec gates for broken commands.
    #[arg(long)]
    no_gate_check: bool,
    /// Preview against this exact store index (plain files only).
    #[arg(long, visible_alias = "at", conflicts_with = "set")]
    at_index: Option<u64>,
    /// Drain changed and retiring seats, then resume their native conversation.
    #[arg(long, value_parser = ["when-idle"], requires = "set")]
    rollout: Option<String>,
    #[arg(long, default_value = "30m", requires = "rollout")]
    rollout_deadline: String,
    /// Interrupt busy work at the deadline; identity and session fences still apply.
    #[arg(long, requires = "rollout")]
    force_after_deadline: bool,
    #[arg(long = "adopt", requires = "set")]
    adopt: Vec<String>,
    #[arg(long, requires = "set")]
    allow_empty: bool,
    #[arg(long, requires = "set")]
    confirm_retire: Option<String>,
    /// Complete person or agent subject authoring the publication.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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

/// Bundle each versioned file into one intent so references may cross file boundaries.
fn read_apply_files(files: &[PathBuf], allow_empty: bool) -> Result<IntentInput> {
    anyhow::ensure!(
        !files.is_empty() || allow_empty,
        "no input files: intentional empty owned membership needs --set and --allow-empty"
    );
    anyhow::ensure!(
        files.iter().filter(|p| p.as_os_str() == "-").count() <= 1,
        "stdin may appear only once"
    );
    let mut bundle = String::from("version 2\n");
    for path in files {
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
    Ok(IntentInput {
        kdl: bundle,
        source_name: Some(
            files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ),
    })
}

async fn run_apply(client: &Client, args: ApplyArgs, json_output: bool) -> Result<()> {
    let intent = read_apply_files(&args.files, args.allow_empty)?;
    if args.set.is_some() {
        return run_owned_set_apply(client, intent, args, json_output).await;
    }
    publish_mission_intent(
        client,
        intent,
        MissionPublishArgs {
            file: PathBuf::new(),
            dry_run: args.dry_run,
            at_index: args.at_index,
            actor: args.actor,
            workspace: args.workspace,
            no_gate_check: args.no_gate_check,
            check: args.check,
            inputs: args.inputs,
        },
        json_output,
    )
    .await
}

async fn run_owned_set_apply(
    client: &Client,
    intent: IntentInput,
    args: ApplyArgs,
    json_output: bool,
) -> Result<()> {
    use st3::store::owned_sets::{Options, Preview, Request, Source};
    let options = Options {
        set: args.set.context("owned publication needs --set")?,
        source: Source {
            repository: args.repository.context("--set needs --repository")?,
            r#ref: args.source_ref.context("--set needs --ref")?,
            sha: args.sha.context("--set needs --sha")?,
            sequence: args
                .source_sequence
                .context("--set needs --source-sequence")?,
        },
        expected_set: args.expect_set.context("--set needs --expect-set")?,
        rollout: args
            .rollout
            .map(|_| {
                st3::graph::parse_duration(&args.rollout_deadline, true).map(|duration| {
                    st3::rollout::Policy::when_idle(duration, args.force_after_deadline)
                })
            })
            .transpose()?,
        adopt: args.adopt.into_iter().collect(),
        allow_empty: args.allow_empty,
        confirm_retire: args.confirm_retire,
        expected_subjects: Default::default(),
    };
    let mut request = Request {
        intent,
        options,
        actor: args.actor,
        idempotency_key: uuid::Uuid::now_v7().to_string(),
    };
    let preview: Preview = client.post("/v1/sets/preview", &request).await?;
    if args.dry_run {
        print_value(&preview, json_output)?;
    }
    anyhow::ensure!(
        preview.blockers.is_empty(),
        "owned set refused: {}",
        preview.blockers.join("; ")
    );
    // Empty owned membership has no gates and is not a valid plain gate-check intent.
    if !preview.empty && (args.check || (!args.dry_run && !args.no_gate_check)) {
        check_before_publish(
            client,
            &request.intent,
            &args.workspace,
            &args.inputs,
            args.check,
        )
        .await?;
    }
    if args.dry_run {
        return Ok(());
    }
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

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
#[derive(Args)]
struct AgentApplyArgs {
    /// KDL file to publish; use `-` to read standard input.
    file: PathBuf,
    /// Complete person or agent subject authoring the publication.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// Print the resolved publication preview without applying or running exec gates.
    #[arg(long, visible_alias = "preview")]
    dry_run: bool,
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases

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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Host)))]
    host: Option<String>,
    /// Record permission to start this placement before unreachable source hosts acknowledge exit.
    #[arg(long, conflicts_with = "print_kdl")]
    source_offline: bool,
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Host)))]
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
    /// Existing repository on the selected host to create a Git worktree from.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Ref to start a new branch from; defaults to origin/main.
    #[arg(long, requires = "repo")]
    base: Option<String>,
    /// Git branch; defaults to the simple agent name made safe for Git. Reuses an existing branch.
    #[arg(long, requires = "repo")]
    branch: Option<String>,
    /// Remove a clean worktree after the seat is stopped; keep its branch and unfinished work.
    #[arg(long, requires = "repo")]
    remove_at_run_end: bool,
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: Option<String>,
}

#[derive(Args)]
struct AgentRetireArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
    subject: String,
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
    reason: String,
    #[arg(long, value_parser = clap::builder::NonEmptyStringValueParser::new())]
    idempotency_key: String,
}

#[derive(Args)]
struct AgentStopArgs {
    /// Exact seat subject or its identity without the `agent/` prefix.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: true })))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// Print the exact stop KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
}

#[derive(Args)]
struct AgentRolloutArgs {
    #[arg(value_parser = parse_agent_start_identity)]
    subject: String,
    #[arg(long = "as", env = "ST_AGENT", value_parser = parse_publication_actor)]
    actor: String,
    #[arg(long, default_value = "30m")]
    deadline: String,
    #[arg(long)]
    force_after_deadline: bool,
}

#[derive(Args)]
struct AgentSuspendArgs {
    /// Exact seat subject or its identity without the `agent/` prefix.
    #[arg(value_parser = parse_agent_start_identity)]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: true })))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    /// Resume the suspended conversation on this fleet host.
    #[arg(long)]
    host: Option<String>,
    /// Exact seat subject or its identity without the `agent/` prefix.
    #[arg(value_parser = parse_agent_start_identity)]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_publication_actor)]
    actor: String,
    /// How long to wait for a new running incarnation.
    #[arg(long, default_value = "10m")]
    timeout: String,
    /// Restart at once. By default a seat that is up and ready is first asked for its handoff and
    /// restarted at its acknowledgement or after two minutes; use this for a seat that cannot answer.
    #[arg(long)]
    now: bool,
}

#[derive(Subcommand)]
enum GhCommand {
    /// Watch an issue or pull request. Each new comment or review that this seat did not post,
    /// each time the required checks on its current head turn pass or fail, changes to its
    /// head, draft state or queue membership, and its close or merge wake this seat once;
    /// the close or merge, or the deadline, ends the watch. Watching
    /// it again keeps the watch and takes the new deadline.
    #[command(
        after_help = "Examples:\n  st gh watch acme/garden#12\n  st gh watch https://github.com/acme/garden/pull/12 --until 4h\n  st gh watch acme/garden#12 --until none"
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
    /// End after 14 days by default; use a duration such as 4h, an RFC 3339 time, or none for no deadline.
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    /// Register an immutable typed custom schema and bounded projection.
    Register {
        file: PathBuf,
        #[arg(long = "as",value_parser=parse_actor_subject)]
        actor: String,
    },
    /// List registered custom kinds and their pinned revisions.
    Registrations,
    /// Read an exact custom registration, KIND@HASH.
    Registration { kind: String },

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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
    #[arg(long)]
    reason: String,
}

#[derive(Subcommand)]
enum AttentionCommand {
    /// List current alerts, then unread updates.
    Ls {
        /// Follow current collection changes.
        #[arg(long, conflicts_with_all = ["all", "cursor"])]
        watch: bool,
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Person)))]
        #[arg(long = "as", value_parser = parse_person_subject)]
        actor: Option<String>,
        /// Include resolved and historical items.
        #[arg(long)]
        all: bool,
        /// Resume the next bounded page returned by an earlier list.
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Explain one alert or update and show the exact available actions.
    Show {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Attention)))]
        subject: String,
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Person)))]
        #[arg(long = "as", value_parser = parse_person_subject)]
        actor: Option<String>,
    },
    /// Chat about an alert: send a message to the agent whose conversation it belongs to, titled
    /// after it, with a reference to what it is about. This is what "Chat about this" does in stui.
    Discuss(AttentionDiscussArgs),
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
struct AttentionDiscussArgs {
    /// The item to talk about: its `attention/...` ID from `st alerts ls`.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Attention)))]
    subject: String,
    /// What to say. The message also names the item, so the agent knows what it is about.
    #[arg(short = 'm', long)]
    body: String,
    /// Who to talk to. By default the agent that asked for the item.
    #[arg(long)]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
    to: Option<String>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Person)))]
    #[arg(long = "as", value_parser = parse_person_subject)]
    actor: Option<String>,
    /// Name this message for retries, as `conversations send --idempotency-key` does.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct AttentionRequestArgs {
    #[arg(long = "for", value_parser = parse_person_subject)]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Person)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Any))))]
    step: Option<String>,
    /// Only a person closes the item, because nothing st observes can say it is done.
    #[arg(long, conflicts_with_all = ["until", "step"])]
    person_closes: bool,
}

#[derive(Args)]
struct AttentionResolveArgs {
    #[command(flatten)]
    delegation: DelegationArgs,
    /// The update's original work.person-asked claim ID.
    #[arg(long, requires = "acted_for")]
    episode: Option<String>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Attention)))]
    subject: String,
    #[arg(long, value_parser = ["resolved", "dismissed"])]
    outcome: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args)]
struct AttentionWithdrawArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Attention)))]
    subject: String,
    #[arg(long)]
    reason: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: String,
}

#[derive(Subcommand)]
enum WorkCommand {
    /// Replace your allowed delegation list; repeat --action, or omit all to revoke.
    Delegation(DelegationPolicyArgs),
    /// Open a one-step run for this seat without authoring a mission; then use claim.
    ///
    /// For a small independent authorized job: describe the result, use the returned claim
    /// command, perform the work and complete it with durable evidence. Finish or release
    /// other independent claimed work first. See st skill for the full workflow.
    Start(WorkStartArgs),
    /// Release claimed work to another seat or person with a note they acknowledge.
    Handoff(WorkHandoffArgs),
    /// Acknowledge the exact handoff note before claiming or completing its work.
    Acknowledge(WorkAcknowledgeArgs),

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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    Show {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Any))))]
        subject: String,
        /// This read needs no actor. Accepted for scripts and ignored with one notice.
        #[arg(long = "as", value_name = "ACTOR")]
        ignored_actor: Option<String>,
    },
    /// Acquire one ready work item with the current harness incarnation.
    #[command(mut_arg("subject", |arg| arg.add(ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Ready))))))]
    Claim(WorkActionArgs),
    /// Extend the live lease for work this incarnation still owns.
    #[command(mut_arg("subject", |arg| arg.add(ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))))]
    Renew(WorkActionArgs),
    /// Record a material progress update without changing ownership.
    ///
    /// Records progress in the graph without messaging another agent; people read it in stui.
    #[command(mut_arg("subject", |arg| arg.add(ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))))]
    Progress(WorkActionArgs),
    /// Add time to the execution budget of claimed work that ran out of it.
    Extend(WorkExtendArgs),
    /// Stop the execution clock while claimed work waits for something outside this seat.
    ///
    /// A build slot or the merge queue is waiting, not execution: the step's timeout counts
    /// only the time it runs. The clock restarts at `work resume`, or after `--for` at the
    /// latest, so a seat that stops answering cannot hide behind a wait. The lease still
    /// needs its renewals.
    Wait(WorkWaitArgs),
    /// Start the execution clock again after `work wait`.
    #[command(mut_arg("subject", |arg| arg.add(ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))))]
    Resume(WorkActionArgs),
    /// Finish claimed work and attach its durable evidence.
    ///
    /// Include the result, exact revision, verification and material limits. Gates can keep
    /// the step verifying after submission. A dependent review step becomes ready when it
    /// completes; review feedback can become the next authorized step or run. See st skill.
    #[command(mut_arg("subject", |arg| arg.add(ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))))]
    Complete(WorkActionArgs),
    /// Fail claimed work with an actionable reason and evidence.
    #[command(mut_arg("subject", |arg| arg.add(ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))))]
    Fail(WorkActionArgs),
    /// Give claimed work back so another eligible agent can take it.
    #[command(mut_arg("subject", |arg| arg.add(ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))))]
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
struct WorkStartArgs {
    /// What this spontaneous task is for.
    title: String,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
    /// Reuse a key after a timeout to recover the same run.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct WorkHandoffArgs {
    subject: String,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
    #[arg(long, env = "ST3_INCARNATION")]
    incarnation: Option<String>,
    #[arg(long)]
    to: String,
    #[arg(long)]
    note: String,
    #[arg(long)]
    evidence: Vec<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct WorkAcknowledgeArgs {
    subject: String,
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
    /// The message ID from the handoff note or work show.
    #[arg(long)]
    message: String,
}

#[derive(Args)]
struct WorkAskArgs {
    #[arg(long = "for")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Person)))]
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
    /// Show the person an image with the request (PNG, JPEG, GIF or WebP, at most 10 MiB, up
    /// to 4). It is uploaded to this member and read by the person through the ask.
    #[arg(long = "attach", value_name = "FILE", requires = "request")]
    attach: Vec<PathBuf>,
    #[arg(long, conflicts_with = "new_run")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Any))))]
    step: Option<String>,
    #[arg(long, conflicts_with = "step")]
    new_run: Option<String>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Person)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", env = "ST_AGENT")]
    actor: String,
    #[arg(long)]
    idempotency_key: String,
}

#[derive(Args)]
struct WorkDoneArgs {
    #[command(flatten)]
    delegation: DelegationArgs,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Any))))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Ready))))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long, default_value = "manual wake requested")]
    reason: String,
}

#[derive(Args)]
struct WorkRetryArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Failed))))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long)]
    reason: String,
}

#[derive(Subcommand)]
enum WorkRevisionCommand {
    /// Show the current revision proposal for one mission run.
    Show {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: false })))]
        run: String,
    },
    /// List every immutable generation created for one mission run.
    Generations {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: false })))]
        run: String,
    },
    /// Explain one exact generation and its work state.
    Generation { generation: String },
    /// Approve an exact proposal preview and permit its cutover.
    Approve {
        proposal: String,
        preview_hash: String,
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
        #[arg(long = "as")]
        actor: Option<String>,
    },
    /// Cancel a pending revision proposal without changing the live generation.
    Cancel {
        proposal: String,
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
        #[arg(long = "as")]
        actor: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Args)]
struct WorkExtendArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
struct WorkWaitArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
    #[arg(long, env = "ST3_INCARNATION")]
    incarnation: Option<String>,
    /// The longest the clock stays stopped, such as 30m or 4h; at most 12h.
    #[arg(long = "for")]
    wait_for: String,
    /// What the step waits for, such as "build slot" or "merge queue".
    #[arg(long)]
    reason: String,
}

#[derive(Args)]
struct WorkActionArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Any))))]
    subject: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::MissionRun { unfinished_only: true })))]
    run: String,
    file: PathBuf,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Work(WorkFilter::Claimed))))]
    subject: String,
    file: PathBuf,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
        #[arg(long = "as", value_parser = parse_actor_subject)]
        actor: Option<String>,
        #[arg(long)]
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
        agent: Option<String>,
        /// Include entries at or after this RFC3339 timestamp.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Send one durable normalized message to an agent.
    ///
    /// A message is a direct connection: it wakes the recipient agent for a full turn,
    /// which rereads its context. Use `--kind wake` (the default) for a question, answer or conversational handoff, `st work handoff` for work, and `--kind silent` for anything
    /// else, which wakes nobody and reaches the recipient at its next turn. Status goes to
    /// `work progress` (it lands in the graph and wakes nobody), run events to report-to.
    /// People have no inbox: a send or reply to a person fails. To reach a person, print in
    /// the chat.
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
    /// which rereads its context. A reply that only acknowledges or reports is `--kind silent`.
    Reply(MessageReplyArgs),
    /// Wait up to a deadline for a reply from one sender, and print the reply or `no reply`.
    ///
    /// The reply is marked read for the waiting seat, so native delivery does not hand it over
    /// again as a separate turn. Without `--after` only messages sent after the wait began count.
    Wait(MessageWaitArgs),
    /// Close exact messages after their related action is complete.
    Archive(MessageArchiveArgs),
    /// Archive unread mail past an age threshold as each recipient.
    Cleanup {
        #[arg(long, conflicts_with = "actor", required_unless_present = "actor")]
        all: bool,
        #[arg(long = "as", conflicts_with = "all", required_unless_present = "all")]
        actor: Option<String>,
        #[arg(long, default_value = "1h")]
        older_than: String,
        /// Show the matching message IDs without archiving them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Render the bounded conversation thread around one message.
    Thread(MessageReferenceArgs),
    /// List normalized harness sessions available for native conversation views.
    Sessions {
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Session)))]
        session: String,
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Session)))]
        session: String,
        #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Agent { running_only: false })))]
    to: String,
    #[arg(short = 'm', long)]
    body: String,
    #[arg(long)]
    subject: Option<String>,
    #[arg(long)]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Message)))]
    in_reply_to: Option<String>,
    #[arg(long, value_delimiter = ',')]
    tags: Vec<String>,
    #[arg(long = "from", alias = "as")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    from: String,
    #[arg(
        long = "attach",
        value_name = "FILE",
        help = st3::attach_image::HELP,
        long_help = st3::attach_image::LONG_HELP
    )]
    attach: Vec<PathBuf>,
    #[command(flatten)]
    wake: MessageWakeArgs,
    /// Print the generated message mission KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
    /// Name this message for retries. Without it the key comes from the sender, recipient, words
    /// and attachments, so running the same command again within the hour (or the next) reports
    /// the message already sent instead of sending it twice.
    #[arg(long)]
    idempotency_key: Option<String>,
}

/// Whether a message wakes its recipient. A person's message, a handoff, a fault, a ready step
/// and a gh watch event always wake it.
#[derive(Args)]
struct MessageWakeArgs {
    /// Wake the recipient (default), or hold unread mail for its next real turn.
    #[arg(long, value_parser = ["silent", "wake"], default_value = "wake")]
    kind: String,
}

impl Default for MessageWakeArgs {
    fn default() -> Self {
        Self {
            kind: "wake".into(),
        }
    }
}

impl MessageWakeArgs {
    fn tags(&self) -> impl Iterator<Item = String> {
        (self.kind == "silent")
            .then(|| st3::silent::SILENT_TAG.to_owned())
            .into_iter()
    }
}

#[derive(Args)]
struct MessageListArgs {
    /// Mailbox identity; defaults to the non-empty ST_AGENT value.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    identity: Option<String>,
    /// The same mailbox identity, spelled like `conversations read --as`.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", conflicts_with = "identity")]
    actor: Option<String>,
    #[arg(long)]
    archive: bool,
    #[arg(long)]
    count: bool,
    #[arg(long = "from")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    sender: Option<String>,
}

#[derive(Args)]
struct MessageReadArgs {
    #[arg(num_args = 1..)]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Message)))]
    references: Vec<String>,
    #[arg(long)]
    raw: bool,
    #[arg(long)]
    archive: bool,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct MessageReplyArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Message)))]
    reference: String,
    #[arg(short = 'm', long)]
    body: String,
    #[arg(long)]
    subject: Option<String>,
    #[arg(long = "from", alias = "as")]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    from: String,
    #[arg(
        long = "attach",
        value_name = "FILE",
        help = st3::attach_image::HELP,
        long_help = st3::attach_image::LONG_HELP
    )]
    attach: Vec<PathBuf>,
    #[command(flatten)]
    wake: MessageWakeArgs,
    /// Print the generated reply mission KDL without publishing it.
    #[arg(long)]
    print_kdl: bool,
    /// Name this reply for retries. Without it the key comes from the sender, the message
    /// replied to, the words and attachments, as for `send`.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct MessageWaitArgs {
    /// The seat waiting for the reply; defaults to the non-empty ST_AGENT value.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
    /// Whose reply to wait for, for example `person/ada`.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "from")]
    sender: String,
    /// Count a reply sent after this message, for a question sent just before the wait began.
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Message)))]
    #[arg(long)]
    after: Option<String>,
    /// How long to wait, such as 30s or 2m, at most 10m (issue the wait again for longer). Zero
    /// checks once and returns.
    #[arg(long, default_value = "30s")]
    timeout: String,
}

#[derive(Args)]
struct MessageArchiveArgs {
    #[command(flatten)]
    delegation: DelegationArgs,
    /// Original message.sent claim ID when acting for a person.
    #[arg(long, requires = "acted_for")]
    episode: Option<String>,
    #[arg(num_args = 1..)]
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Message)))]
    references: Vec<String>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as")]
    actor: Option<String>,
}

#[derive(Args)]
struct MessageReferenceArgs {
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Message)))]
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
    #[command(flatten)]
    delegation: DelegationArgs,
    /// Exact gate.requested claim ID when acting for a person.
    #[arg(long, requires = "acted_for")]
    episode: Option<String>,
    /// The gate to answer: its `attention/...` ID from `st alerts ls`, or the step, mission
    /// or loop run (`step-run/...`, `mission-run/...`, `loop-run/...`) that owns it.
    target: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
}

#[derive(Args, Default)]
struct DelegationArgs {
    /// Record a person's existing instruction, with yourself as the actor.
    #[arg(long = "for", value_parser = parse_person_subject, requires_all = ["policy", "instruction", "quote", "episode"])]
    acted_for: Option<String>,
    /// Claim ID of the person's current person.delegation-set policy.
    #[arg(long, requires = "acted_for")]
    policy: Option<String>,
    /// The person's original message/ID instructing this action.
    #[arg(long, requires = "acted_for")]
    instruction: Option<String>,
    /// The person's instruction, quoted verbatim.
    #[arg(long, requires = "acted_for")]
    quote: Option<String>,
}

impl DelegationArgs {
    fn proof(self, episode: Option<&str>) -> Result<Option<st3::model::DelegationProof>> {
        let Some(person) = self.acted_for else {
            return Ok(None);
        };
        Ok(Some(st3::model::DelegationProof {
            person,
            policy: self.policy.context("delegation needs --policy")?,
            message: format!(
                "message/{}",
                normalize_message_reference(
                    &self.instruction.context("delegation needs --instruction")?,
                )
            ),
            quote: self.quote.context("delegation needs --quote")?,
            episode: episode.context("delegation needs --episode")?.into(),
        }))
    }
}

#[derive(Args)]
struct DelegationPolicyArgs {
    #[arg(long = "for", value_parser = parse_person_subject)]
    person: String,
    #[arg(long = "as", value_parser = parse_actor_subject)]
    actor: String,
    #[arg(long = "action", value_parser = ["answer-ask", "close-item", "record-go-stop"])]
    actions: Vec<String>,
    #[arg(long, required = true)]
    evidence: Vec<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct FeedbackReviewArgs {
    /// The feedback gate to answer: its `attention/...` ID from `st alerts ls`, or the
    /// step run that owns it.
    target: String,
    #[arg(long)]
    reason: String,
    #[arg(add = ArgValueCompleter::new(Complete(Entity::Actor)))]
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
    /// OMP transcript selected only for the durable first native launch.
    #[arg(long)]
    seed: Option<PathBuf>,
    #[arg(last = true)]
    argv: Vec<String>,
}

#[derive(Subcommand)]
enum AdmissionCommand {
    /// Explicitly allow this installed build without running its admission probes.
    Override {
        #[command(flatten)]
        build: AdmissionBuildArgs,
        #[arg(long)]
        reason: String,
    },
    /// Revoke this installed build's exception; retained measurements apply again.
    Revoke {
        #[command(flatten)]
        build: AdmissionBuildArgs,
    },
}

#[derive(Args)]
struct AdmissionBuildArgs {
    #[arg(value_parser = ["omp", "opencode"])]
    harness: String,
    /// Exact executable used by the affected seat; defaults to the harness on PATH.
    #[arg(long)]
    binary: Option<String>,
    /// This machine's st state directory; defaults to the configured state directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

fn run_admission(command: AdmissionCommand, json_output: bool) -> Result<()> {
    anyhow::ensure!(
        std::env::var_os("ST_AGENT").is_none(),
        "admission exceptions must be managed from a person's terminal, outside an agent seat"
    );
    let (build, reason) = match command {
        AdmissionCommand::Override { build, reason } => (build, Some(reason)),
        AdmissionCommand::Revoke { build } => (build, None),
    };
    let state_dir = match build.state_dir {
        Some(path) => path,
        None => Config::load_unvalidated(None)?.state_dir,
    };
    let cache = state_dir.join("drivers/sessions/harness-admission");
    let driver = if build.harness == "omp" {
        st_drivers::driver_diagnostic::Driver::Omp
    } else {
        st_drivers::driver_diagnostic::Driver::OpenCode
    };
    // Use the immutable asset the managed launch also exports. Installing hooks here would
    // mutate a managed seat; this disposable file only supplies the adapter identity bytes.
    let adapter = tempfile::NamedTempFile::new()?;
    let extension = if driver == st_drivers::driver_diagnostic::Driver::Omp {
        let (_, bytes) = st3::hooks::FILES
            .iter()
            .find(|(name, _)| *name == "omp-channel.ts")
            .expect("the managed hook set includes omp");
        fs::write(adapter.path(), bytes)?;
        Some(adapter.path())
    } else {
        None
    };
    let binary = build.binary.as_deref().unwrap_or(&build.harness);
    let path = if let Some(reason) = &reason {
        st_drivers::harness_admission::override_build(binary, driver, extension, &cache, reason)?
    } else {
        st_drivers::harness_admission::revoke_override(binary, driver, extension, &cache)?
    };
    if json_output {
        println!(
            "{}",
            json!({"harness":build.harness,"executable":binary,"overridden":reason.is_some(),"reason":reason,"record":path})
        );
    } else if reason.is_some() {
        println!(
            "{} installed build explicitly allowed; admission checks bypassed. Record: {}\nRestart only the affected seat to apply. A changed executable or shipped extension needs its own exception. Revoke with `st admission revoke {} --binary {}`.",
            build.harness,
            path.display(),
            build.harness,
            shell_argument(binary)
        );
    } else {
        println!(
            "{} installed-build exception revoked. Retained measurements apply on the next launch. Record: {}",
            build.harness,
            path.display()
        );
    }
    Ok(())
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

fn read_contract_inventory_for_command(mut command: clap::Command) -> Value {
    fn walk(
        command: &clap::Command,
        prefix: &str,
        output: &mut BTreeSet<String>,
        aliases: &mut BTreeMap<String, Vec<String>>,
    ) {
        let children = command
            .get_subcommands()
            .filter(|child| child.get_name() != "help")
            .collect::<Vec<_>>();
        if prefix != "st" && (children.is_empty() || !command.is_subcommand_required_set()) {
            output.insert(prefix.to_owned());
        }
        for child in children {
            let path = format!("{prefix} {}", child.get_name());
            let mut names = child
                .get_all_aliases()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            names.sort();
            names.dedup();
            if !names.is_empty() {
                aliases.insert(path.clone(), names);
            }
            walk(child, &path, output, aliases);
        }
    }

    command.build();
    let mut commands = BTreeSet::new();
    let mut aliases = BTreeMap::new();
    walk(&command, "st", &mut commands, &mut aliases);
    json!({
        "kind": "st3.cli-inventory.v1",
        "modes": ["human", "json"],
        "commands": commands,
        "aliases": aliases,
    })
}

fn read_contract_inventory_json() -> Value {
    read_contract_inventory_for_command(Cli::command())
}

fn read_contract_inventory_request(arguments: &[std::ffi::OsString]) -> Option<bool> {
    const FLAG: &str = "--read-contract-inventory-json";
    let present = arguments
        .iter()
        .skip(1)
        .take_while(|argument| argument.as_os_str() != std::ffi::OsStr::new("--"))
        .any(|argument| {
            argument == FLAG || argument.to_string_lossy().starts_with(&format!("{FLAG}="))
        });
    present.then(|| arguments.len() == 2 && arguments[1] == FLAG)
}

fn main() -> ExitCode {
    cli_output::finish(cli_main())
}

fn cli_main() -> ExitCode {
    // A recorder link starts st3 as `git` or `gh`. It must not build the async runtime.
    if let Some(program) = st3::recorder::invoked_program() {
        st3::recorder::run(program);
    }
    let arguments = std::env::args_os().collect::<Vec<_>>();
    match read_contract_inventory_request(&arguments) {
        Some(true) => {
            println!("{}", read_contract_inventory_json());
            return ExitCode::SUCCESS;
        }
        Some(false) => {
            eprintln!("st: --read-contract-inventory-json must be used alone");
            return ExitCode::from(2);
        }
        None => {}
    }
    // An instance changes where every path resolves, so it comes before anything reads one.
    let instance_name = st3::instance::requested(&arguments)
        .or_else(|| std::env::var(st3::instance::ENV).ok().filter(|name| !name.is_empty()));
    if let Some(name) = instance_name {
        // SAFETY: no other thread exists yet; the async runtime starts after this returns.
        if let Err(error) = unsafe { st3::instance::activate(&name) } {
            eprintln!("st: --instance {name}: {error:#}");
            return ExitCode::from(2);
        }
    }
    // A shell stub from `st completions` calls back with `COMPLETE=<shell>` on each TAB. Answer
    // before any config, runtime, or daemon work; this exits when the variable is set.
    clap_complete::env::CompleteEnv::with_factory(Cli::command).complete();
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
    // Cargo bakes the target name into each executable. The installed st3 binary cannot
    // enable this with an argument, environment variable, or a different filename.
    #[cfg(feature = "test-support")]
    let _fixture_shell = (env!("CARGO_BIN_NAME") == "st3-fixture")
        .then(st3::test_support::initialize_fixture);
    // SAFETY: no other thread exists yet; the async runtime starts after this returns.
    unsafe { st_drivers::reexec::take_resume_environment() };
    if st_drivers::reexec::resume_path(st_drivers::reexec::DRIVER_RESUME_ENV).is_some() {
        // The predecessor blocked the stop signals across its exec. Install the handlers before
        // unblocking them, so a stop that arrived in between ends the session the ordinary way.
        st_drivers::provider_session::install_stop_handlers();
        st_drivers::reexec::unblock_stop_signals();
    }
    if plain_ui_requested(
        arguments.len(),
        std::io::stdin().is_terminal(),
        cli_output::stdout().is_terminal(),
        std::env::var_os("ST_AGENT").is_some(),
    ) {
        let subject = match crate::setup::prepare_plain_ui() {
            Ok(subject) => subject,
            Err(error) => {
                eprintln!("st: {}", plain_error(&error));
                return ExitCode::FAILURE;
            }
        };
        let options = match subject {
            Some(subject) => stui::Args::default().with_initial_subject(subject),
            None => stui::Args::default(),
        };
        return run_terminal_ui(options, None);
    }
    if cli_help::all_help_requested(&arguments) {
        print!("{}", cli_help::root_help(true));
        return ExitCode::SUCCESS;
    }
    let json_version = arguments.iter().any(|arg| arg == "--json");
    let matches = match Cli::command()
        .override_help(cli_help::root_help(false))
        .try_get_matches_from(arguments)
    {
        Ok(matches) => matches,
        Err(error) if error.kind() == clap::error::ErrorKind::DisplayVersion && json_version => {
            println!(
                "{}",
                json!({ "machine_version": st_drivers::version::machine_version() })
            );
            return ExitCode::SUCCESS;
        }
        Err(error) => exit_usage_error(error),
    };
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| exit_usage_error(error));
    // Root runs tiles. Entering one makes namespaces, which needs this process single-threaded,
    // so it runs before any runtime or thread starts.
    if let Command::Tiles(args) = cli.command {
        return match st3::tiles::cli::run(args) {
            Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
            Err(error) => {
                eprintln!("st tiles: {error:#}");
                ExitCode::FAILURE
            }
        };
    }
    if let Command::Ui(options) = cli.command {
        return run_terminal_ui(options, cli.endpoint.as_deref());
    }
    // Set the CLI identity only after the TUI path has chosen its stui client identity.
    st3_client::set_client_name(format!("st {}", st_drivers::version::machine_version()));
    if let Command::Up(args) = &cli.command {
        record_daemon_commands(args);
    }
    match driver_environment_incarnation(&cli) {
        Ok(Some(incarnation)) => {
            // SAFETY: the single-threaded lookup runtime has been dropped; run_cli starts
            // the multi-threaded runtime only after this environment update.
            unsafe { std::env::set_var("ST3_INCARNATION", incarnation) };
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("st: {error:#}");
            return ExitCode::FAILURE;
        }
    }
    run_cli(
        cli,
        matches.subcommand_name().expect("a subcommand was parsed"),
    )
}

/// Bare st opens the TUI only for a person using both sides of a terminal.
fn plain_ui_requested(arguments: usize, stdin: bool, stdout: bool, seat: bool) -> bool {
    arguments == 1 && stdin && stdout && !seat
}

/// The TUI owns its runtime and terminal restoration; never enter it from run_cli.
fn run_terminal_ui(mut options: stui::Args, endpoint: Option<&str>) -> ExitCode {
    let result = (|| -> Result<()> {
        if let Some(endpoint) = endpoint {
            match Endpoint::parse(endpoint) {
                Endpoint::Unix(path) => options = options.with_endpoint(path),
                _ => anyhow::bail!(
                    "st ui uses a local Unix socket; pair remote devices with st devices complete"
                ),
            }
        }
        stui::run(options)
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("st: {}", plain_error(&error));
            ExitCode::FAILURE
        }
    }
}

/// Export the runtime fence before any provider or runtime worker thread starts. Fresh
/// launches replace an inherited fence; an in-place driver re-exec keeps the saved runtime.
fn driver_environment_incarnation(cli: &Cli) -> Result<Option<String>> {
    let Command::Driver(args) = &cli.command else {
        return Ok(None);
    };
    let subject = args.subject.clone().or_else(|| {
        (args.driver != "claude-mcp")
            .then(|| args.identity.as_deref().map(normalize_agent_subject))
            .flatten()
    });
    if args.driver == "claude-mcp" {
        let scoped = subject
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || std::env::var("ST3_SUBJECT")
                .ok()
                .is_some_and(|value| !value.trim().is_empty());
        anyhow::ensure!(
            scoped || args.identity.is_none(),
            "the Claude channel identity requires a subject"
        );
    }
    let Some(subject) = subject.as_deref() else {
        return Ok(None);
    };
    // The exec driver also runs gates and resource tasks. Those processes do not own
    // an agent runtime and must start without waiting for a seat incarnation.
    if !subject.starts_with("agent/") {
        return Ok(None);
    }
    if let Some(path) = st_drivers::reexec::resume_path(st_drivers::reexec::DRIVER_RESUME_ENV) {
        let resume: DriverResume = st_drivers::reexec::peek_state(&path)?;
        anyhow::ensure!(
            resume.subject == subject && resume.driver == args.driver,
            "the driver resume state belongs to another seat or driver"
        );
        return Ok(Some(resume.incarnation));
    }
    if let Some(path) = st_drivers::reexec::resume_path(st_drivers::reexec::CHANNEL_RESUME_ENV) {
        let resume: Value = st_drivers::reexec::peek_state(&path)?;
        let incarnation = resume
            .get("incarnation")
            .or_else(|| resume.pointer("/fence/incarnation"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("the channel resume state has no runtime incarnation")?;
        return Ok(Some(incarnation.to_owned()));
    }
    // Serve scoped Claude MCP initialization while asynchronous bootstrap discovers
    // and binds ownership. Local argument and resume validation above still applies.
    if args.driver == "claude-mcp" {
        return Ok(None);
    }
    let config = Config::load_client_unvalidated(None)?.0;
    let endpoint = cli
        .endpoint
        .clone()
        .or_else(|| std::env::var("ST3_ENDPOINT").ok())
        .as_deref()
        .map(Endpoint::parse)
        .unwrap_or_else(|| Endpoint::Unix(config.client_socket()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // Exec seats share the daemon's environment but do not have a PTY entry.
    let use_local_pty_registry = args.driver != "exec" && has_local_pty_registry();
    runtime
        .block_on(wait_for_agent_incarnation_from(
            &Client::new(endpoint),
            subject,
            use_local_pty_registry,
        ))
        .map(Some)
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

// Writer-bound store work admits before spawn_blocking. A global blocking-thread cap
// cannot replace that admission: native provider sessions occupy pool threads for their
// lifetime, and their control callbacks can depend on another blocking worker.
#[tokio::main]
async fn run_cli(cli: Cli, command_name: &str) -> ExitCode {
    let native_driver = matches!(&cli.command, Command::Driver(_));
    if native_driver {
        st3::telemetry::local_only();
    }
    let _telemetry = (!matches!(
        &cli.command,
        Command::Driver(_) | Command::Up(_) | Command::ReplicationWorker(_)
    ))
    .then(|| st3::otel::Telemetry::init(st3::otel::Unit::Cli, None));
    let command_span = if st3::otel::export_enabled() {
        tracing::info_span!("st3.cli.command", span.label = command_name)
    } else {
        tracing::Span::none()
    };
    let result = {
        use tracing::Instrument as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let result = run(cli).instrument(command_span.clone()).await;
        if result.as_ref().is_err_and(|error| {
            !error
                .downcast_ref::<CommandExit>()
                .is_some_and(|exit| exit.0 == 0)
        }) {
            command_span.set_status(opentelemetry::trace::Status::error("command failed"));
        }
        result
    };
    drop(command_span);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(exit) = error.downcast_ref::<CommandExit>() {
                return ExitCode::from(exit.0);
            }
            let message = if native_driver {
                plain_error(&error)
            } else {
                doctor_cli::human_outage_message(&error)
                    .unwrap_or_else(|| plain_error(&error))
            };
            eprintln!("st: {message}");
            if refused_command(&error) {
                st3::gate_report::note_refusal(&plain_error(&error));
            }
            let message = error.to_string();
            if error.downcast_ref::<EventCursorGap>().is_some() {
                ExitCode::from(6)
            } else if daemon_is_unreachable(&error) {
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
    if let Command::Setup(args) = cli.command {
        return crate::setup::run(args).await.map(|_| ());
    }
    // Completion is a remote device operation. It needs neither a local daemon nor its config.
    if let Command::Devices(DevicesArgs {
        command:
            Some(DevicesCommand::Complete {
                member_url,
                pairing_id,
                algorithm,
                key_file,
                profile,
                allow_public_http,
                fingerprint,
                unpinned,
            }),
        ..
    }) = &cli.command
    {
        return run_devices_complete(
            member_url,
            pairing_id,
            algorithm,
            key_file.as_deref(),
            profile.as_deref(),
            st3_client::device::CompletionOptions {
                allow_public_http: *allow_public_http,
                fingerprint: fingerprint.as_deref(),
                unpinned: *unpinned,
            },
            cli.json,
        )
        .await;
    }
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
    // Evidence is portable: repository objects and a caller trust list suffice.
    if let Command::Evidence(args) = cli.command {
        return st3::evidence::cli::run(args);
    }
    // The gateway runs as the sekrets user, which has no st configuration.
    if let Command::Sekrets(args) = cli.command {
        let code = st3::sekrets::cli::run(args, cli.json, cli.endpoint).await?;
        use std::io::Write as _;
        let _ = cli_output::stdout().flush();
        return Err(CommandExit(code as u8).into());
    }
    if let Command::Admission { command } = cli.command {
        return run_admission(command, cli.json);
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
        st3::node_identity::resolve(&mut config)?;
        let mut telemetry =
            st3::otel::Telemetry::init(st3::otel::Unit::ReplicationWorker, Some(&config.node));
        let result = st3::peer::run_worker(config).await;
        telemetry.shutdown();
        return result;
    }
    let (config, unknown_config_keys) = Config::load_client_unvalidated(None)?;
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
        Command::Setup(_) => unreachable!(),
        Command::Apply(args) => run_apply(&client, args, cli.json).await,
        Command::Sets { command } => run_owned_sets(&endpoint, command, cli.json).await,
        Command::Ui(_) => unreachable!("the TUI runs before the CLI runtime"),
        Command::Up(_) => unreachable!(),
        Command::Skill(_) => unreachable!(),
        Command::Evidence(_) => unreachable!(),
        Command::Sekrets(_) => unreachable!(),
        Command::Tiles(_) => unreachable!(),
        Command::Admission { .. } => unreachable!(),
        Command::ReplicationWorker(_) => unreachable!(),
        Command::Now(args) => run_now(&endpoint, config.person.as_deref(), args, cli.json).await,
        Command::Note(args) => {
            directive_note::run(&endpoint, config.person.as_deref(), args, cli.json).await
        }
        Command::Usage(args) => run_usage(&immediate, args, cli.json).await,
        Command::Slo(args) => run_slo(&immediate, args, cli.json).await,
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
        Command::Clients => run_clients(&endpoint, cli.json).await,
        Command::Work { command } => run_work(&client, &endpoint, command, cli.json).await,
        Command::Changes(arguments) => {
            let client = if matches!(
                &arguments.command,
                change_cli::ChangesCommand::List { .. } | change_cli::ChangesCommand::Show { .. }
            ) {
                client
            } else {
                let actor = match std::env::var("ST_AGENT")
                    .ok()
                    .filter(|agent| agent.starts_with("agent/"))
                {
                    Some(agent) => agent,
                    None => parse_person_subject(config.person.as_deref().context(
                        "Git change sync needs a configured person or the current agent seat",
                    )?)
                    .map_err(anyhow::Error::msg)?,
                };
                let Endpoint::Unix(socket) = &endpoint else {
                    anyhow::bail!("Git change control commands require the trusted local Unix socket");
                };
                let named = if actor.starts_with("agent/") {
                    Client::unix_agent(socket, actor)?
                } else {
                    Client::unix_as(socket, actor)?
                };
                named.with_outage_wait(DAEMON_WAIT.get().copied().unwrap_or_default(), true)
            };
            change_cli::run(&client, arguments, cli.json).await
        }
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
        Command::Doctor(args) => run_doctor(&immediate, args, cli.json, &unknown_config_keys).await,
        Command::Recorder { command } => run_recorder(command, &config, cli.json),
        Command::Repair { command } => run_repair(&client, command, cli.json).await,
        Command::Backup { command } => match command {
            BackupCommand::Create {
                file,
                database: None,
            } => {
                st3::backup::create(&endpoint, &file).await?;
                if cli.json {
                    println!("{}", json!({"file":file}));
                } else {
                    println!("Backup saved to {}", file.display());
                }
                Ok(())
            }
            BackupCommand::Create {
                file,
                database: Some(database),
            } => {
                let header = st3::backup::create_from_database(&database, &file)?;
                if cli.json {
                    println!("{}", serde_json::to_string(&header)?);
                } else {
                    println!(
                        "Backup saved to {} (graph {})",
                        file.display(),
                        header.graph_digest
                    );
                }
                Ok(())
            }
            BackupCommand::Restore { file, database } => {
                let database = database.unwrap_or_else(|| config.state_dir.join("claims.sqlite3"));
                let report = st3::backup::restore(&file, &database)?;
                if cli.json {
                    println!("{}", serde_json::to_string(&report)?);
                } else {
                    println!(
                        "Restored {} envelopes to {}\nGraph {}\nConfigure node = \"{}\" before starting this database.",
                        report.envelopes,
                        database.display(),
                        report.graph_digest,
                        report.writer
                    );
                }
                Ok(())
            }
        },
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
                CompletionShell::Bash => "bash",
                CompletionShell::Zsh => "zsh",
                CompletionShell::Fish => "fish",
            };
            clap_complete::env::Shells::builtins()
                .completer(shell)
                .context("clap_complete lacks a built-in shell")?
                .write_registration(
                    "COMPLETE",
                    "st",
                    "st",
                    "st",
                    &mut cli_output::stdout(),
                )?;
            Ok(())
        }
        Command::Driver(args) => run_driver(&immediate, args, cli.catalog.as_deref()).await,
        Command::Gate { command } => {
            if matches!(&command, GateCommand::Merged { .. } | GateCommand::CiPassed { .. }) {
                st3::resource::configure_github(&config)?;
            }
            run_gate(command).await
        },
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
            // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
            MissionViewCommand::Publish(args) => Some(args.actor.as_str()),
            // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases
            MissionViewCommand::Start(args) => Some(args.actor.as_str()),
            MissionViewCommand::Cancel(args) => Some(args.actor.as_str()),
            MissionViewCommand::Outcome(args) => Some(args.actor.as_str()),
            MissionViewCommand::ReportTo(args) => Some(args.actor.as_str()),
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
            // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
            AgentsCommand::Apply(args) => Some(args.actor.as_str()),
            // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases
            AgentsCommand::Start(args) => Some(args.actor.as_str()),
            AgentsCommand::Stop(args) => Some(args.actor.as_str()),
            AgentsCommand::Retire(args) => Some(args.actor.as_str()),
            AgentsCommand::Restart(args) => Some(args.actor.as_str()),
            AgentsCommand::Rollout(args) => Some(args.actor.as_str()),
            AgentsCommand::Suspend(args) => Some(args.actor.as_str()),
            AgentsCommand::Resume(args) => Some(args.actor.as_str()),
            AgentsCommand::AcknowledgeSeed(args) => Some(args.actor.as_deref().ok_or_else(|| {
                anyhow::anyhow!("a harness seed acknowledgement needs explicit --as {own}")
            })?),
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
            WorkCommand::Delegation(args) => Some(args.actor.as_str()),
            WorkCommand::Done(args) | WorkCommand::CancelAsk(args) => Some(args.actor.as_str()),
            WorkCommand::Start(args) => Some(args.actor.as_str()),
            WorkCommand::Handoff(args) => Some(args.actor.as_str()),
            WorkCommand::Acknowledge(args) => Some(args.actor.as_str()),
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
        Command::Conversations {
            command: MessageCommand::Archive(args),
        } => args.actor.as_deref(),
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

#[derive(Debug)]
struct PrivateGatewayCollision {
    derived: PathBuf,
}

impl std::fmt::Display for PrivateGatewayCollision {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            output,
            "pass --client-gateway-socket; the derived private gateway {} equals the shared default gateway",
            self.derived.display()
        )
    }
}

impl std::error::Error for PrivateGatewayCollision {}

fn gateway_path_for_comparison(socket: &Path) -> Result<PathBuf> {
    use std::path::Component;

    let socket = std::path::absolute(socket)?;
    let parent = socket.parent().context("gateway socket has no parent")?;
    let mut ancestor = parent;
    let mut resolved = loop {
        match fs::canonicalize(ancestor) {
            Ok(resolved) => break resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .context("gateway socket has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("resolve gateway socket parent"),
        }
    };
    let mut missing_depth = 0_usize;
    for component in parent.strip_prefix(ancestor)?.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
                if missing_depth > 0 {
                    missing_depth -= 1;
                } else {
                    // Walk from the physical ancestor, not the lexical symlink location.
                    resolved = fs::canonicalize(&resolved)
                        .context("resolve gateway socket ancestor after ..")?;
                }
            }
            Component::Normal(name) => {
                resolved.push(name);
                if missing_depth > 0 {
                    missing_depth += 1;
                    continue;
                }
                // A .. can return to an existing directory; resolve symlinks again there.
                match fs::canonicalize(&resolved) {
                    Ok(path) => resolved = path,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        match fs::symlink_metadata(&resolved) {
                            Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {
                                missing_depth = 1;
                            }
                            Ok(_) => {
                                return Err(error).context("resolve existing gateway socket ancestor");
                            }
                            Err(error) => {
                                return Err(error).context("inspect gateway socket ancestor");
                            }
                        }
                    }
                    Err(error) => return Err(error).context("resolve gateway socket ancestor"),
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                anyhow::bail!("gateway socket suffix must be relative");
            }
        }
    }
    Ok(resolved.join(
        socket
            .file_name()
            .context("gateway socket has no file name")?,
    ))
}

fn select_private_gateway(
    config: &mut Config,
    private_state: bool,
    private_socket: bool,
) -> Result<()> {
    if !(private_state || private_socket) {
        return Ok(());
    }
    let parent = if private_state {
        Some(config.state_dir.as_path())
    } else {
        config
            .socket
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
    };
    config.client_gateway_socket = parent
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("st3-client.sock");
    if gateway_path_for_comparison(&config.client_gateway_socket)?
        == gateway_path_for_comparison(&Config::default().client_gateway_socket)?
    {
        return Err(PrivateGatewayCollision {
            derived: config.client_gateway_socket.clone(),
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod private_gateway_tests;

fn daemon_start_version(machine_version: &str) -> Result<&str> {
    anyhow::ensure!(
        machine_version.len() <= 128,
        "daemon build identity exceeds 128 bytes; correct the compile-time build stamp"
    );
    Ok(
        if machine_version == concat!(env!("CARGO_PKG_VERSION"), "+dev") {
            env!("CARGO_PKG_VERSION")
        } else {
            machine_version
        },
    )
}

async fn run_up(args: UpArgs) -> Result<()> {
    let started_at = Instant::now();
    let mut phase_finished_at = started_at;
    let mut phase_elapsed_ms = || {
        let now = Instant::now();
        let elapsed_ms = now.duration_since(phase_finished_at).as_secs_f64() * 1_000.0;
        phase_finished_at = now;
        elapsed_ms
    };
    let mut startup_phases = st3::startup::PhaseDurations::default();
    let args_config = args.config.clone();
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
        select_private_gateway(&mut config, private_state, private_socket)?;
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
    let configured_node = config.node.clone();
    let identity_started = std::time::SystemTime::now();
    let startup_started = std::time::Instant::now();
    let state_identity = st3::node_identity::acquire(&mut config);
    let identity_ended = std::time::SystemTime::now();
    let identity_duration = startup_started.elapsed();
    let mut telemetry = st3::otel::Telemetry::init(
        st3::otel::Unit::Daemon,
        Some(if state_identity.is_ok() { &config.node } else { &configured_node }),
    );
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    st3::malloc_trim::spawn()?;
    let startup_telemetry =
        st3::startup_telemetry::StartupTelemetry::begin_at(identity_started, startup_started);
    startup_telemetry.node_identity(
        identity_started,
        identity_ended,
        identity_duration,
        state_identity.is_err(),
    );
    let _state_identity = match state_identity {
        Ok(identity) => identity,
        Err(error) => {
            drop(startup_telemetry);
            telemetry.shutdown();
            return Err(error);
        }
    };
    config.validate()?;
    st3::resource::configure_github(&config)?;
    validate_unix_socket_path(&config.socket, "--socket")?;
    validate_unix_socket_path(&config.client_gateway_socket, "--client-gateway-socket")?;
    fs::create_dir_all(&config.state_dir)?;
    let startup = Arc::new(st3::startup::Startup::begin(&config.socket)?);
    startup_phases.configuration_ms = phase_elapsed_ms();
    startup.phase("bind-listeners");
    let state_socket = config.state_dir.join("run/st3.sock");
    let (local_reserved, gateway_reserved) = tokio::try_join!(
        st3::api::reserve_unix_socket(&config.socket, Some(&state_socket)),
        st3::api::reserve_unix_socket(&config.client_gateway_socket, None),
    )?;
    // Only addresses are bound. Until initialization finishes, connections are
    // refused and clients use the existing live startup-phase observation.
    let reserved_bind_ms = phase_elapsed_ms();
    startup.phase("install-hooks");
    startup_telemetry.phase("install-hooks");
    st3::hooks::ensure_installed(&st3::hooks::root(&config.state_dir)).context(
        "publishing this st binary's required lifecycle hook set before starting the daemon",
    )?;
    st3::profile::init_from_env();
    raise_open_file_limit();
    startup_phases.install_hooks_ms = phase_elapsed_ms();
    startup.phase("open-store");
    startup_telemetry.phase("open-store");
    let store = Arc::new(startup_telemetry.task("startup open-store", || {
        Store::open(&config.state_dir.join("claims.sqlite3"), &config.node)
    })?);
    if let Some(fleet_id) = &config.fleet_id {
        store.bind_fleet(fleet_id)?;
    }
    // A member pins its anchor, applies its writer floor, and signs with its key before it
    // writes anything, so every local batch after this point is signed.
    if let Some(file) = &config.fleet {
        smallclaims::fleet::prepare(&store, &config.state_dir, file)?;
    }
    // Every claim this node writes is signed; keys are made here the first time, silently.
    let keys = st3::fleet::join::key_directory(&config.state_dir);
    if config.fleet.is_none() {
        store.set_node_key(Arc::new(st3::fleet::join::standalone_node_key(
            &config.state_dir,
        )?))?;
    }
    store.use_key_directory(&keys)?;
    startup_phases.open_store_ms = phase_elapsed_ms();
    startup.phase("judge-claims");
    startup_telemetry.phase("judge-claims");
    startup_telemetry.task("startup judge-claims", || store.judge_claims(true))?;
    startup_phases.judge_claims_ms = phase_elapsed_ms();
    startup.phase("validate-replication-backlog");
    startup_telemetry.phase("validate-replication-backlog");
    let admission = startup_telemetry.task("startup validate-replication-backlog", || {
        store.validate_replication_backlog()
    })?;
    startup_phases.validate_replication_backlog_ms = phase_elapsed_ms();
    startup.phase("apply-replication-repairs");
    startup_telemetry.phase("apply-replication-repairs");
    startup_telemetry.task("startup apply-replication-repairs", || {
        store.apply_replication_repairs()
    })?;
    startup_phases.apply_replication_repairs_ms = phase_elapsed_ms();
    startup.phase("settle-runs");
    startup_telemetry.phase("settle-runs");
    for run in startup_telemetry.task("startup settle-runs", || {
        store.settle_runs_for_canonical_replay()
    })? {
        eprintln!("st: mission run `{run}` stays over as this node's graph showed it");
    }
    startup.phase("project-replication-backlog");
    startup_telemetry.phase("project-replication-backlog");
    let projected = startup_telemetry.task("startup project-replication-backlog", || {
        store.project_replication_backlog_with_progress(
            "startup/project-replication-backlog",
            |progress| {
                startup_telemetry.progress(progress);
                startup.progress(progress);
            },
        )
    }).context("startup projection failed; stopping before daemon.started and runtime initialization")?;
    if !projected {
        eprintln!(
            "st: the replicated projection remains deferred; the daemon will use its last good graph"
        );
    }
    if admission.unknown != 0 {
        eprintln!("st: {} claims waiting for a newer build", admission.unknown);
    }
    if admission.invalid != 0 {
        eprintln!("st: replication has {} invalid records", admission.invalid);
    }
    startup_phases.settlement_projection_ms = phase_elapsed_ms();
    startup.phase("initialize-runtime");
    startup_telemetry.phase("initialize-runtime");
    store.append_claim(&ClaimInput {
        subject: format!("daemon/{}", config.node),
        kind: "daemon.started".into(),
        actor: None,
        fields: BTreeMap::from([
            (
                "features".into(),
                serde_json::json!({"owned_sets":1,"seat_rollout":1,"seat_rollout_manual":1,"person_directive_note":1,
                    "harness_access_auto": u8::from(config.active_access_mode() == st_drivers::access::AccessMode::Auto)}),
            ),
            ("status".into(), Value::String("running".into())),
            ("pid".into(), Value::from(std::process::id())),
            (
                "version".into(),
                Value::String(daemon_start_version(&st_drivers::version::machine_version())?.into()),
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
    startup_phases.runtime_prelude_ms = phase_elapsed_ms();
    let login_environment = st3::environment::snapshot_at_startup()?;
    startup_phases.login_environment_ms = phase_elapsed_ms();
    let isolation_environment = login_environment.clone();
    tokio::task::spawn_blocking(move || st_runtime::initialize_isolation(&isolation_environment))
        .await
        .context("probe task isolation during daemon startup")?;
    let pty_binary = match args.pty_binary.clone() {
        Some(pty_binary) => pty_binary,
        None => st_runtime::resolve_executable("pty", &login_environment)?,
    };
    startup_phases.isolation_pty_ms = phase_elapsed_ms();
    let recorder = install_recorder(&config, &login_environment);
    startup_phases.recorder_ms = phase_elapsed_ms();
    if let Some(person) = &config.person {
        let _ = st3::sekrets::daemon::PERSON.set(person.clone());
    }
    // Long message bodies this machine owns that no claim names any longer are removed.
    st3::message_body::spawn_reconciler(store.clone(), config.state_dir.clone());
    // Sekrets is opt-in: this records a gateway's calls once one listens on this host.
    st3::sekrets::daemon::spawn_importer(
        store.clone(),
        config.node.clone(),
        config.state_dir.clone(),
        st3::sekrets::client::socket_path(),
    );
    store.install_git_change_registry(Arc::new(st3::changes::service::Registry::new(&config.changes, &config.node)?))?;
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
    // Tiles this daemon's user supervises reach it only through their restricted endpoint.
    #[cfg(target_os = "linux")]
    st3::api::spawn_tile_endpoints(state.clone());
    st3::gate_check::set_endpoint(config.socket.display().to_string());
    startup_phases.app_state_ms = phase_elapsed_ms();
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
    )?.with_access_mode(config.active_access_mode()).with_seat_priority(config.active_seat_priority()).with_codex_asks(config.codex_work_without_asking == Some(false)).with_schedule_peers(state.configured_peers.clone()).with_client_relay(state.client_relay.clone()).with_person(config.person.clone()));
    reconciler.set_max_passes_per_minute(config.reconcile.max_passes_per_minute)?;
    reconciler.set_idle_nudge(config.reconcile.idle_nudge()?);
    startup_phases.reconciler_ms = phase_elapsed_ms();
    startup.phase("start-services");
    startup_telemetry.phase("start-services");
    tokio::spawn(reconciler.clone().supervise());
    tokio::spawn(st3::profile::watch_runtime_lag());
    // The policy reads `[limits]` again on every pass, so an edit applies without a restart.
    st3::config::set_daemon_config(args_config.as_deref());
    tokio::spawn(enforce_account_limits(store.clone(), config.limits.clone(), reconciler));
    recycle_idle_wal(store.path.clone(), Arc::downgrade(&store));
    let _contention_retry = retry_projection_contention(Arc::downgrade(&store), notify.clone(), event_notify.clone(), config.state_dir.clone());
    tokio::spawn(convert_envelope_payloads(store.clone()));
    tokio::spawn(migrate_event_payloads(store.clone()));
    if let Some(interval) = st3::maintenance::divergence_check_interval(
        std::env::var(st3::maintenance::DIVERGENCE_CHECK_ENV).ok().as_deref(),
    ) {
        tokio::spawn(st3::maintenance::check_projection_divergences(store.clone(), interval));
    }
    tokio::spawn(catch_up_coordination_counts(store.clone()));
    tokio::spawn(copy_stop_deferral_history(store.clone()));
    spawn_response_expiry(store.clone());
    tokio::spawn(trim_local_observations(
        store.clone(),
        config.observations.clone(),
    ));
    tokio::spawn(catch_up_account_limits(store.clone(), notify.clone()));
    {
        let store = store.clone();
        tokio::spawn(st3::slo_history::run(
            st3::slo_history::dir(&config.state_dir),
            move || st3::api::slo_rows(&store),
        ));
    }
    tokio::spawn(st3::recorder_receipts::run(
        store.clone(),
        st3::recorder::receipt_path(&config.state_dir),
        notify.clone(),
        event_notify.clone(),
    ));
    if let Some(otlp) = &config.observations.otlp {
        let exporter = st3::otlp::OtlpExporter::new(otlp, &config.node)?;
        eprintln!(
            "st3: exporting local observations to OpenTelemetry at {}",
            otlp.endpoint
        );
        tokio::spawn(st3::otlp::run(store.clone(), exporter));
        // The live SLO windows go out as gauges, once a minute, on a timer of their own.
        tokio::spawn(st3::otlp::run_slo(st3::otlp::OtlpExporter::new(otlp, &config.node)?));
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
    // The first diagnostic report and delayed repair share one operation audit. Later reports
    // audit current state independently; no request waits for this startup report.
    if let Some(audit) = st3::api::start_operation_report(&state) {
        let store = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let drift = match audit.await {
                Ok(Ok(drift)) => drift,
                Ok(Err(error)) => {
                    eprintln!("st3: operation projection check failed: {error:#}");
                    return;
                }
                Err(error) => {
                    eprintln!("st3: operation projection check stopped: {error}");
                    return;
                }
            };
            match store.connection.write_workers().spawn_blocking(move || {
                store.repair_operation_projection_drift_from_audit(&drift)
            })
            .await
            {
                Ok(Ok(true)) => eprintln!("st3: rebuilt an operation projection that drifted"),
                Ok(Ok(false)) => {}
                Ok(Err(error)) => eprintln!("st3: operation projection check failed: {error:#}"),
                Err(error) => eprintln!("st3: operation projection check stopped: {error}"),
            }
        });
    }
    // Nor does the first session list wait to read every native transcript's header.
    st3::api::start_native_session_discovery(&state);
    // Nor does the first agents roster read fold every agent's card.
    st3::api::start_agent_roster(&state);
    // Nor does an attention window fold every person's attention.
    st3::api::start_attention_list(&state);
    // Nor does a missions window fold the missions it shows.
    st3::api::start_published_lists(&state);
    startup_phases.background_tasks_ms = phase_elapsed_ms();
    let bind_started_at = phase_finished_at;
    startup.phase("bind-listeners");
    startup_telemetry.phase("bind-listeners");
    // Fabric forwards each terminal tunnel to this socket once `st terminals expose-fabric` names
    // it. Without it the daemon still serves everything else, and attaches use the gateway.
    let terminal_socket = st3::config::terminal_socket(&config.state_dir);
    let _terminal_socket = match std::path::absolute(&state.pty_root)
        .map_err(anyhow::Error::from)
        .and_then(|pty_root| st3::terminal_fabric::TerminalSocket::start(&terminal_socket, pty_root))
    {
        Ok(service) => {
            eprintln!(
                "st: Fabric terminal socket listening at {}",
                terminal_socket.display()
            );
            Some(service)
        }
        Err(error) => {
            eprintln!(
                "st: Fabric terminal socket {} is unavailable, so attaches from other hosts use the client gateway: {error:#}",
                terminal_socket.display()
            );
            None
        }
    };
    let bound = std::sync::atomic::AtomicUsize::new(0);
    let ready = || {
        if bound.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            let activation_store = store.clone();
            let checkpoint_context = config
                .checkpoint
                .enabled
                .then(|| st3::store::CheckpointContext {
                    now_unix_ms: 0,
                    configured_peers: config.peers.iter().map(|peer| peer.name.clone()).collect(),
                    scratch: config.state_dir.join("checkpoint"),
                    reviewer: config
                        .person
                        .clone()
                        .unwrap_or_else(|| "person/operator".into()),
                });
            tokio::spawn(async move {
                match activation_store.connection.write_workers().spawn_blocking({
                    let store = activation_store.clone();
                    move || store.complete_replication_activation()
                })
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        eprintln!("st: replication history activation failed: {error:#}");
                        return;
                    }
                    Err(error) => {
                        eprintln!("st: replication history activation stopped: {error}");
                        return;
                    }
                }
                if let Some(context) = checkpoint_context {
                    run_checkpoints(activation_store, context).await;
                }
            });
            spawn_publication_evidence(store.clone());
            startup_telemetry.serving();
            let ready_at = Instant::now();
            startup_phases.log_ready(
                reserved_bind_ms + ready_at.duration_since(bind_started_at).as_secs_f64() * 1_000.0,
                ready_at.duration_since(started_at).as_secs_f64() * 1_000.0,
            );
            startup.serving();
            eprintln!("st: local API listening at {}", config.socket.display());
            eprintln!(
                "st: paired client gateway listening at {}",
                config.client_gateway_socket.display()
            );
        }
    };
    tokio::try_join!(
        st3::api::serve_reserved_unix_with_ready(
            local_reserved,
            router(state.clone()),
            true,
            ready
        ),
        st3::api::serve_reserved_unix_with_ready(gateway_reserved, fabric_router(state), false, ready),
    )?;
    telemetry.shutdown();
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

fn notice_ignored_read_actor(actor: Option<&str>) {
    if actor.is_some() {
        eprintln!("st: --as is ignored for this read; no actor is required");
    }
}

// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-10-st-apply-aliases — DELETE at contraction — https://www.notion.so/3f5e3d41f4a3818aadb6f4b74030c1da
async fn publish_mission_file(
    client: &Client,
    args: MissionPublishArgs,
    json_output: bool,
) -> Result<()> {
    let (kdl, source_name) = read_intent(Some(&args.file))?;
    let intent = IntentInput { kdl, source_name };
    publish_mission_intent(client, intent, args, json_output).await
}
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-10-st-apply-aliases

async fn publish_mission_intent(
    client: &Client,
    intent: IntentInput,
    args: MissionPublishArgs,
    json_output: bool,
) -> Result<()> {
    let mission: MissionResponse = client
        .post(
            "/v1/intent/mission",
            &MissionRequest {
                intent: intent.clone(),
                at_index: args.at_index,
            },
        )
        .await?;
    if args.dry_run {
        print_value(&mission, json_output)?;
    }
    anyhow::ensure!(
        mission.blockers.is_empty(),
        "{}",
        mission.blockers.join("; ")
    );
    if args.check || (!args.dry_run && !args.no_gate_check) {
        check_before_publish(
            client,
            &mission.resolved_intent,
            &args.workspace,
            &args.inputs,
            args.check,
        )
        .await?;
    }
    if args.dry_run {
        return Ok(());
    }
    warn_ignored_authority(&mission);
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

/// Check mission and step exec gates before publication, listing loop until scripts as unchecked.
async fn check_before_publish(
    client: &Client,
    intent: &IntentInput,
    workspace: &Path,
    inputs: &[(String, String)],
    required: bool,
) -> Result<()> {
    let mut announced = false;
    let checked = run_gate_check(client, intent, workspace, inputs, |view, item| {
        if !announced {
            eprintln!("{}", gate_check_heading(view));
            announced = true;
        }
        eprint!("{}", render_gate_check_item(item));
    })
    .await?;
    let Some(view) = checked else {
        anyhow::ensure!(
            !required,
            "this st daemon cannot check exec gates; update it"
        );
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

/// Check the exec gates of `intent` on the daemon and wait for every answer, including deferred
/// loop until scripts, handing each one to `report` as it arrives. `None` for older daemons.
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

/// `ms` in its largest whole unit: `90s`, `45m`, `2h`.
fn whole_duration(ms: u64) -> String {
    match ms {
        ms if ms >= 3_600_000 && ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
        ms if ms >= 60_000 && ms % 60_000 == 0 => format!("{}m", ms / 60_000),
        ms if ms % 1_000 == 0 => format!("{}s", ms / 1_000),
        ms => format!("{ms}ms"),
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
    let run_id = mission_start_run_id(mission_id, args.id.as_deref());
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
        &run_id,
        mission_id,
        &mission.revision,
        &workspace,
        &requester,
        &inputs,
        "run",
        after.as_deref(),
        args.report_to.as_deref(),
        args.report_completed,
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
    follow_mission_run(client, started, response.store_index, json_output, 50).await
}

fn mission_start_run_id(mission_id: &str, requested: Option<&str>) -> String {
    match requested {
        None => format!("{mission_id}/{}", uuid::Uuid::now_v7().simple()),
        Some(requested) => {
            let id = requested.strip_prefix("mission-run/").unwrap_or(requested);
            if id.contains('/') {
                id.to_owned()
            } else {
                format!("{mission_id}/{id}")
            }
        }
    }
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
    run: MissionRunView,
    _cursor: u64,
    json_output: bool,
    limit: usize,
) -> Result<()> {
    let interactive = cli_output::stdout().is_terminal();
    let _screen = if !json_output && interactive {
        Some(TerminalScreen::open()?)
    } else {
        None
    };
    follow_mission_run_to(
        client, run, json_output, interactive, limit, OutputStyle::stdout(), &mut cli_output::stdout(),
    ).await
}

async fn follow_mission_run_to(
    client: &Client,
    mut run: MissionRunView,
    json_output: bool,
    interactive: bool,
    limit: usize,
    style: OutputStyle,
    output: &mut impl std::io::Write,
) -> Result<()> {
    let client = client.clone().with_follow_retry();
    let client = &client;
    let mut prior = String::new();
    loop {
        if cli_output::closed() {
            return Ok(());
        }
        let page = if json_output {
            None
        } else {
            Some(load_mission_run_tree(client, &run, None, limit).await?)
        };
        let summary = if let Some(page) = &page {
            format!(
                "{}:{}:{:?}:{}",
                run.updated_at_unix_ms,
                page.has_more,
                page.next_cursor,
                mission_run_signature(&page.runs)?
            )
        } else {
            String::new()
        };
        if summary != prior && let Some(page) = &page {
            let mut frame = render_mission_run_page(&run, &page.runs, style, current_unix_ms()?);
            if let Some(cursor) = &page.next_cursor {
                frame.push_str(&format!(
                    "\nTREE      More runs follow; st missions show {} --cursor {cursor} --limit {limit}\n",
                    run.subject,
                ));
            }
            write!(
                output,
                "{}",
                follow_snapshot(&frame, interactive, !prior.is_empty())
            )?;
            output.flush()?;
            prior = summary;
        }
        client.follow_recovered();
        match run.status.as_str() {
            status if mission_run_follow_succeeded(status) => {
                return if json_output {
                    writeln!(output, "{}", serde_json::to_string_pretty(&run)?)?;
                    Ok(())
                } else {
                    Ok(())
                };
            }
            "failed" | "cancelled" => {
                anyhow::bail!("mission run {} is {}", run.subject, run.status)
            }
            _ => {}
        }
        if cli_output::closed() {
            return Ok(());
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
    after: Option<&str>,
    limit: usize,
) -> Result<st3::model::MissionRunTreePage> {
    let mut query = format!(
        "/v1/mission-runs/tree?root={}&limit={limit}",
        urlencoding::encode(&selected.root_mission_run)
    );
    if let Some(after) = after {
        query.push_str("&after=");
        query.push_str(&urlencoding::encode(after));
    }
    client
        .get(&query)
        .await
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
    report_to: Option<&str>,
    report_completed: bool,
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
    if let Some(report_to) = report_to {
        body.nodes_mut().push(kdl_node("report-to", [report_to]));
        if report_completed {
            body.nodes_mut()
                .push(kdl_node("report-completed", ["true"]));
        }
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
    budget: Option<tokio::time::Instant>,
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
    let deadline = budget.unwrap_or_else(|| tokio::time::Instant::now() + LOCAL_ATTACH_CONSULT);
    let answer = match tokio::time::timeout_at(deadline, consult.as_mut()).await {
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
            (None, _) if budget.is_some() => {
                anyhow::bail!("st terminals attach: daemon did not answer within 1 s and no local PTY session matches `{subject}`");
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

/// `st terminals expose-fabric`: have this machine's Fabric forward the fleet's PTY protocol to
/// the st daemon's terminal socket, or with `--exec` run `st terminals serve-fabric` for each
/// tunnel.
async fn expose_fabric(config: &Config, args: PtyExposeFabricArgs) -> Result<()> {
    let (fabric, fleet_id) = configured_fabric()?;
    let fleet_id =
        fleet_id.context("this machine is in no fleet, so no peer can attach its terminals")?;
    let fabric = fabric.context("`fabric` is not on PATH")?;
    let protocol = st3::terminal_fabric::protocol(&fleet_id);
    if args.exec {
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
    } else {
        let socket = std::path::absolute(st3::config::terminal_socket(&config.state_dir))?;
        fabric.expose_socket(&protocol, &socket).await?;
        println!(
            "Fabric forwards `{protocol}` to the st daemon's terminal socket {}.",
            socket.display()
        );
    }
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
    let mut shown = json!({ "status": status, "recent_claims": claims.claims });
    // Who signed a message a person wrote, and whether that checks.
    if args.subject.starts_with("message/")
        && let Some(provenance) = message_provenance(client, &args.subject).await
    {
        shown["provenance"] = provenance;
    }
    print_value(&shown, json_output)
}

async fn run_trace(client: &Client, args: TraceArgs, json_output: bool) -> Result<()> {
    run_trace_to(client, args, json_output, &mut cli_output::stdout()).await
}

async fn run_trace_to(
    client: &Client,
    args: TraceArgs,
    json_output: bool,
    output: &mut impl std::io::Write,
) -> Result<()> {
    anyhow::ensure!(
        args.limit > 0 && args.limit <= 500,
        "the trace limit must be 1 through 500"
    );
    let client = if args.follow {
        client.clone().with_follow_retry()
    } else {
        client.clone()
    };
    let client = &client;
    let claims = trace_claims(client, &args).await?;
    let mut cursor = args.after_index.unwrap_or_default();
    for claim in claims {
        if cli_output::closed() {
            return Ok(());
        }
        cursor = cursor.max(claim.store_index);
        if json_output {
            writeln!(output, "{}", serde_json::to_string(&claim)?)?;
        } else {
            write_trace_claim(output, &claim)?;
        }
    }
    if !args.follow || cli_output::closed() {
        return Ok(());
    }
    if args.after_index.is_none() {
        // The bounded history may be empty or older than the global event floor. Default
        // follow starts now; an explicit --after-index still requires continuous replay.
        let health: Value = client.get("/v1/health").await?;
        cursor = health["store_index"]
            .as_u64()
            .context("the daemon health response has no store index")?;
    }
    client.follow_recovered();
    let mut event_feed = LocalEventFeed { trace: true };
    loop {
        if cli_output::closed() {
            return Ok(());
        }
        let mut event_query = vec![
            format!("after={cursor}"),
            "wait=true".into(),
            "timeout_ms=30000".into(),
        ];
        if let Some(subject) = &args.subject {
            event_query.push(format!("subject={}", urlencoding::encode(subject)));
        }
        if let Some(owner_run) = &args.owner_run {
            event_query.push(format!("owner_run={}", urlencoding::encode(owner_run)));
        }
        let page = event_feed.read(client, &event_query.join("&")).await?;
        let scanned = page.next_after;
        for event in page.items {
            if cli_output::closed() {
                return Ok(());
            }
            if json_output {
                writeln!(output, "{}", serde_json::to_string(&event)?)?;
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
                    write_trace_claim(output, &claim)?;
                } else {
                    writeln!(
                        output,
                        "{}\t{}\t{}\t(no claim details)",
                        event.store_index, event.kind, event.subject
                    )?;
                }
            }
            cursor = cursor.max(event.store_index);
        }
        if let Some(scanned) = scanned {
            cursor = cursor.max(scanned);
        }
        client.follow_recovered();
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

fn write_trace_claim(output: &mut impl std::io::Write, claim: &ClaimRecord) -> Result<()> {
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
        writeln!(
            output,
            "{}\t{}\t{}\t{}",
            claim.store_index, timestamp, claim.kind, claim.subject
        )?;
    } else {
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}",
            claim.store_index, timestamp, claim.kind, claim.subject, summary
        )?;
    }
    Ok(())
}

fn trace_scalar(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn generated_client(endpoint: &Endpoint, person: Option<&str>) -> Result<GeneratedClient> {
    let Endpoint::Unix(socket) = endpoint else {
        anyhow::bail!(
            "client-v0 product commands require the trusted local Unix endpoint; remote clients must use a paired Fabric credential"
        );
    };
    if let Some(startup) = st3::startup::read(socket)
        && startup.status == "starting"
        // A bound API can already answer before both listeners have published `serving`.
        // Try the connection before interpreting an observation as an outage.
        && std::os::unix::net::UnixStream::connect(socket).is_err()
    {
        if startup.full_replay() {
            if let Some(outage) = st3::client::DaemonUnreachable::during_startup(socket)
                && outage.full_replay()
            {
                return Err(outage.into());
            }
        } else {
            eprintln!("st: {}", startup.summary());
        }
    }
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
    needs_you.filters.clear();
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
    // `N alerts` and the alerts, then updates; nothing at all when nothing waits.
    output.push_str(&render_alert_sections(
        &needs_you,
        None,
        continuation_command,
    ));
    // The server fills Now with attention, and adds work only for an explicit work
    // filter. Print a section only when the page holds its items, so a section the
    // server never filled does not read as zero.
    for (title, section) in [("WORKING", &working), ("UNHEALTHY", &unhealthy)] {
        if !section.items.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&render_product_page(title, section, continuation_command));
        }
    }
    if working.items.is_empty() && unhealthy.items.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("Work: st work ls · Health: st doctor\n");
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
        if !args.follow || cli_output::closed() {
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
        DevicesCommand::Complete { .. } => unreachable!("handled before loading local config"),
        DevicesCommand::Ls => {
            anyhow::ensure!(
                limit > 0 && limit <= 200,
                "the device limit must be 1 through 200"
            );
            let response = client
                .devices_list(cursor.as_deref(), Some(limit), all)
                .await?;
            let history = if all { " --all" } else { "" };
            if json_output {
                return print_product_page("DEVICES", &response, true, "");
            }
            // Each device says whether it is connected to this member now, or when it was last
            // seen, with the build it reported. A daemon without clients.list says nothing.
            let clients = client.clients_list().await.ok();
            let rendered = render_product_page(
                "DEVICES",
                &response.value,
                &format!("st devices --as {person}{history}"),
            );
            print!(
                "{}",
                with_device_presence(&rendered, clients.as_ref().map(|found| &found.value), now_ms())
            );
            note_partial_page(&response.value);
            Ok(())
        }
        DevicesCommand::Pair {
            device_name,
            full_control,
            read_only,
        } => {
            let response = client
                .pairing_begin(&PairingBegin {
                    api_version: CLIENT_V0_API_VERSION.into(),
                    device_name,
                    person_id: person.clone(),
                    full_control: full_control.then_some(true),
                    scopes: read_only.then(|| {
                        READ_ONLY_PAIRING_SCOPES
                            .iter()
                            .map(|scope| (*scope).to_owned())
                            .collect()
                    }),
                })
                .await?;
            print_client_value(&response, json_output)?;
            if !json_output {
                if let Some(fingerprint) = &response.value.person_root_fingerprint {
                    println!(
                        "Person-root fingerprint: {fingerprint}\nCopy this separately from the pairing code to the new device's fingerprint field or --fingerprint option."
                    );
                } else {
                    eprintln!(
                        "WARNING: This member has no person-root fingerprint; pinned pairing requires a signing-enabled member."
                    );
                }
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

async fn run_devices_complete(
    member_url: &str,
    pairing_id: &str,
    algorithm: &str,
    key_file: Option<&Path>,
    profile: Option<&Path>,
    options: st3_client::device::CompletionOptions<'_>,
    json_output: bool,
) -> Result<()> {
    use st3_client::device::{KeyAlgorithm, SigningKey};
    options.validate()?;
    let algorithm = match algorithm {
        "ed25519" => KeyAlgorithm::Ed25519,
        "p256" => KeyAlgorithm::P256,
        _ => anyhow::bail!("Choose ed25519 or p256"),
    };
    let key = match key_file {
        Some(path) => SigningKey::import(algorithm, path)?,
        None => SigningKey::generate(algorithm)?,
    };
    let path = profile
        .map(Path::to_path_buf)
        .map(Ok)
        .unwrap_or_else(st3_client::device::profile_path)?;
    let code = st3_client::device::read_pairing_code()?;
    let device = st3_client::device::complete_with_options(
        &path, member_url, pairing_id, &code, key, options,
    )
    .await?;
    // Always choose explicit safe fields, including --json. PairedSession contains a bearer.
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "kind": "device-paired", "device_id": device.session.device_id,
                "person_id": device.session.person_id, "scopes": device.session.scopes,
                "expires_at": device.session.expires_at, "profile": path,
                "signing_key": device.signing_key.as_ref().map(SigningKey::public_key).transpose()?,
                "person_root_fingerprint": device.person_root_fingerprint,
            }))?
        );
    } else {
        println!(
            "Paired {} as {}. Saved device credentials{} to {}.",
            device.session.device_id,
            device.session.person_id,
            if device.signing_key.is_some() {
                " and signing key"
            } else {
                ""
            },
            path.display()
        );
    }
    Ok(())
}

async fn run_clients(endpoint: &Endpoint, json_output: bool) -> Result<()> {
    let client = generated_client(endpoint, None)?;
    let response = client.clients_list().await?;
    if json_output {
        return print_value(&response, true);
    }
    print!("{}", render_clients(&response.value, now_ms()));
    Ok(())
}

/// How long ago an RFC 3339 instant was, in words: `just now`, `4m ago`, `2h ago`, `3d ago`.
fn ago(timestamp: &str, now_ms: u128) -> String {
    let Ok(then) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return timestamp.to_owned();
    };
    let seconds = (now_ms as i64 / 1_000).saturating_sub(then.timestamp()).max(0);
    match seconds {
        0..60 => "just now".into(),
        60..3_600 => format!("{}m ago", seconds / 60),
        3_600..86_400 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn render_clients(found: &st3_client::ClientConnections, now_ms: u128) -> String {
    use std::fmt::Write as _;
    let mut out = format!("CLIENTS  {} on {}\n", found.items.len(), found.member);
    for item in &found.items {
        let who = match (&item.device_name, &item.device_id) {
            (Some(name), _) => format!("{} ({name})", item.person),
            (None, Some(device)) => format!("{} ({device})", item.person),
            (None, None) => item.actor.clone(),
        };
        let when = if item.connected {
            format!("connected now, since {}", ago(&item.since, now_ms))
        } else {
            format!("last seen {}", ago(&item.last_seen, now_ms))
        };
        let _ = writeln!(
            out,
            "{}  {who}  via {}  {when}",
            item.client.as_deref().unwrap_or("(unnamed client)"),
            item.via
        );
        if !item.follows.is_empty() {
            let _ = writeln!(out, "  follows: {}", item.follows.join(", "));
        }
    }
    if found.items.is_empty() {
        out.push_str("No client has reached this member in the last few minutes.\n");
    }
    out
}

/// `rendered` (a devices page) with each device's line saying whether it is connected to this
/// member now or when it was last seen, and the build it reported.
fn with_device_presence(
    rendered: &str,
    clients: Option<&st3_client::ClientConnections>,
    now_ms: u128,
) -> String {
    let Some(clients) = clients else {
        return rendered.to_owned();
    };
    let mut out = String::new();
    for line in rendered.lines() {
        out.push_str(line);
        let device = line.split_whitespace().next().unwrap_or_default();
        if device.starts_with("device/")
            && let Some(item) = clients
                .items
                .iter()
                .find(|item| item.device_id.as_deref() == Some(device))
        {
            let when = if item.connected {
                "connected now".to_owned()
            } else {
                format!("last seen {}", ago(&item.last_seen, now_ms))
            };
            out.push_str(&format!(
                "  · {when}{}",
                item.client
                    .as_deref()
                    .map(|client| format!(" · {client}"))
                    .unwrap_or_default()
            ));
        }
        out.push('\n');
    }
    out
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
        if cli_output::closed() {
            return Ok(());
        }
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
                if cli_output::closed() {
                    return Ok(());
                }
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
            if cli_output::stdout().is_terminal() {
                print!("\x1b[2J\x1b[H");
            }
            if collection == "agents" {
                print!(
                    "{}",
                    render_client_agents(&page.value, false, false, "st agents ls --watch")
                );
            } else if collection == "attention" {
                print!(
                    "{}",
                    render_alert_sections(&page.value, Some(title), "st alerts ls --watch")
                );
            } else {
                print_product_page(title, &page, false, &format!("st {collection} ls --watch"))?;
            }
            cli_output::stdout().flush()?;
            if cli_output::closed() {
                return Ok(());
            }
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

/// What kind of thing an attention item is, in the word a person uses for it.
fn attention_word(item: &st3_client::Attention) -> &'static str {
    if item.update.is_some() {
        "update"
    } else if let Some(request) = &item.request {
        match request.entry_type.as_str() {
            "decision" => "decision",
            "choice" => "choice",
            "feedback" => "feedback",
            _ => "request",
        }
    } else if item.launch_id.is_some() {
        "launch"
    } else if item.review_mode.is_some() {
        "review"
    } else if item.message_id.is_some() {
        "message"
    } else {
        "request"
    }
}

/// An attention item's first lines: its kind and title, then who it is from and how long ago,
/// with its priority and state only when they are not the usual normal and open.
fn attention_heading(item: &st3_client::Attention, now_unix_ms: u128) -> String {
    let mut about = Vec::new();
    // The agent whose conversation it belongs to, which a gate or an approval names even though
    // no agent asked it; an older daemon names only the requester.
    if let Some(from) = item.conversation_id.as_ref().or(item.requester_id.as_ref()) {
        about.push(format!("from {}", from.strip_prefix("agent/").unwrap_or(from)));
    }
    about.push(ago(&item.requested_at, now_unix_ms));
    if item.priority != "normal" {
        about.push(format!("{} priority", item.priority));
    }
    if item.state != "open" {
        about.push(item.state.clone());
    }
    format!(
        "\n{:<9} {}\n{:<9} {}\n",
        attention_word(item),
        item.title,
        "",
        about.join(" · ")
    )
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
    render_product_page_headed(
        &format!("{title}  {}", page.items.len()),
        page,
        continuation_command,
    )
}

/// Whether an attention item is an alert: it blocks or waits on its person. A daemon that
/// predates alerts does not say, and then everything but an update is one.
fn is_alert(item: &st3_client::Attention) -> bool {
    item.alert.unwrap_or(item.update.is_none())
}

/// `3 alerts`: the first thing a person reads. No alert prints nothing at all.
fn alerts_heading(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 alert".into()),
        count => Some(format!("{count} alerts")),
    }
}

/// An attention page as a person reads it: its alerts under `heading`, then the updates that
/// ask nothing under their own heading, uncounted as alerts. `heading` gets the alert count;
/// None prints the alerts under `N alerts`, and nothing when there are none.
fn render_alert_sections(
    page: &ClientPage,
    heading: Option<&str>,
    continuation_command: &str,
) -> String {
    let attention = |item: &ClientResource| matches!(item, ClientResource::Attention(_));
    let alert = |item: &ClientResource| matches!(item, ClientResource::Attention(item) if is_alert(item));
    let mut alerts = page.clone();
    alerts.items.retain(alert);
    alerts.page.next_cursor = None;
    let mut updates = page.clone();
    updates.items.retain(|item| attention(item) && !alert(item));
    updates.page.next_cursor = None;
    updates.filters.clear();
    let mut output = match heading {
        Some(heading) => render_product_page(heading, &alerts, continuation_command),
        None => alerts_heading(alerts.items.len())
            .map(|heading| render_product_page_headed(&heading, &alerts, continuation_command))
            .unwrap_or_default(),
    };
    if !updates.items.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&render_product_page("UPDATES", &updates, continuation_command));
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

fn render_product_page_headed(
    heading: &str,
    page: &ClientPage,
    continuation_command: &str,
) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    let _ = writeln!(output, "{heading}");
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
                output.push_str(&attention_heading(item, now_ms()));
                for target in &item.target_states {
                    let _ = writeln!(output, "  {}", attention_target_line(target, now_ms()));
                }
                // Opening an update is reading it: it leaves the person's home, so the line says so.
                let reads = if item.update.is_some() {
                    " (marks it read)"
                } else {
                    ""
                };
                let _ = writeln!(
                    output,
                    "  action{reads}: st alerts show {} --as {}",
                    item.source_id, item.person_id
                );
                // What `st alerts approve` and its siblings take: a person copies it from here.
                let _ = writeln!(output, "  id: {}", item.header.id);
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
    let path = if args.messages_only {
        "/v1/usage/messages"
    } else {
        "/v1/usage"
    };
    let report: Value = client
        .get(&format!("{path}?since_ms={since}&until_ms={until}"))
        .await?;
    if json_output || args.messages_only {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print!("{}", render_usage_report(&report, args.hours, args.by));
    Ok(())
}

async fn run_slo(client: &Client, args: SloArgs, json_output: bool) -> Result<()> {
    let report: Value = client.get("/v1/slo").await?;
    let rows = report["rows"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    match args.command.unwrap_or(SloCommand::Ls) {
        SloCommand::Ls => {
            if json_output {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", render_slo_list(rows));
            }
        }
        SloCommand::Chart { id, over, width } => {
            let row = st3::objectives::find(rows, &id).map_err(|message| anyhow::anyhow!(message))?;
            let period_ms = parse_slo_period(&over)?;
            let until = current_unix_ms()? as u64;
            let since = until.saturating_sub(period_ms);
            let columns = width.unwrap_or_else(|| terminal_columns().unwrap_or(80).saturating_sub(7)).clamp(10, 400);
            let history: Value = client
                .get(&format!(
                    "/v1/slo/history?id={}&since_ms={since}&until_ms={until}&steps={columns}",
                    row["id"].as_str().unwrap_or_default().replace('/', "%2F"),
                ))
                .await?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(&history)?);
            } else {
                print!("{}", render_slo_chart(row, &history, &over));
            }
        }
        SloCommand::Explain { id } => {
            let row = st3::objectives::find(rows, &id).map_err(|message| anyhow::anyhow!(message))?;
            if json_output {
                println!("{}", serde_json::to_string_pretty(row)?);
            } else {
                print!("{}", render_slo_explanation(row));
            }
        }
    }
    Ok(())
}

fn render_slo_list(rows: &[Value]) -> String {
    use std::fmt::Write as _;

    let text = |row: &Value, field: &str| row[field].as_str().unwrap_or_default().to_owned();
    let left = |row: &Value| {
        row["remaining_percent"]
            .as_f64()
            .map_or_else(|| "-".to_owned(), |left| format!("{left:.0}%"))
    };
    let mut output = String::new();
    for (kind, heading) in [
        ("objective", "OBJECTIVES  a target with windows and an error budget"),
        ("budget", "BUDGETS  an allowance that resets"),
    ] {
        let group = rows.iter().filter(|row| row["kind"] == kind).collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "{heading} · {}", group.len());
        let table = group
            .iter()
            .map(|row| {
                [
                    text(row, "status"),
                    text(row, "id"),
                    text(row, "target"),
                    text(row, "current"),
                    text(row, "window"),
                    left(row),
                ]
            })
            .collect::<Vec<_>>();
        let header = ["STATUS", "ID", "TARGET", "CURRENT", "WINDOW", "LEFT"].map(str::to_owned);
        let widths: Vec<usize> = (0..6)
            .map(|column| {
                std::iter::once(&header)
                    .chain(&table)
                    .map(|line| line[column].chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        for line in std::iter::once(&header).chain(&table) {
            let cells = line
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect::<Vec<_>>()
                .join("  ");
            let _ = writeln!(output, "{}", cells.trim_end());
        }
    }
    if output.is_empty() {
        output.push_str("No objectives or budgets.\n");
    }
    output
}

/// A period such as `90m`, `24h` or `7d`, in milliseconds. The daemon keeps seven days.
fn parse_slo_period(text: &str) -> Result<u64> {
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("--over takes a number then m, h or d, such as 24h; got {text:?}"))?;
    let unit_ms = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => anyhow::bail!("--over takes a number then m, h or d, such as 24h; got {text:?}"),
    };
    let period = number.saturating_mul(unit_ms);
    anyhow::ensure!(period >= 60_000, "--over must be at least a minute");
    anyhow::ensure!(period <= 7 * 86_400_000, "the history keeps seven days; --over can be at most 7d");
    Ok(period)
}

/// What is left of one row over a period: columns of equal time, each as tall as its lowest
/// reading, from 0% at the bottom to 100% at the top. A column with no reading is a dot.
fn render_slo_chart(row: &Value, history: &Value, over: &str) -> String {
    use std::fmt::Write as _;

    const HEIGHT: usize = 8;
    const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let steps = history["steps"]
        .as_array()
        .map(|steps| steps.iter().map(Value::as_f64).collect::<Vec<_>>())
        .unwrap_or_default();
    let id = row["id"].as_str().unwrap_or_default();
    let what = if row["kind"] == "budget" { "allowance left" } else { "error budget left" };
    let mut output = String::new();
    let _ = writeln!(output, "{id} · {what} · last {over}");
    let readings = history["samples"].as_u64().unwrap_or(0);
    if readings == 0 {
        let _ = writeln!(
            output,
            "No readings in this period. The daemon records one a minute from when it starts, and keeps seven days."
        );
        return output;
    }
    let known = steps.iter().flatten().copied().collect::<Vec<_>>();
    for line in 0..HEIGHT {
        let label = match line {
            0 => "100%",
            l if l == HEIGHT / 2 => " 50%",
            l if l == HEIGHT - 1 => "  0%",
            _ => "    ",
        };
        let cells = steps
            .iter()
            .map(|step| match step {
                Some(percent) => {
                    let eighths = (percent.clamp(0.0, 100.0) / 100.0 * (HEIGHT * 8) as f64).round() as usize;
                    // Even a spent budget shows its reading as the lowest sliver, never a gap.
                    let eighths = if line == HEIGHT - 1 { eighths.max(1) } else { eighths };
                    LEVELS[eighths.saturating_sub((HEIGHT - 1 - line) * 8).min(8)]
                }
                None if line == HEIGHT - 1 => '·',
                None => ' ',
            })
            .collect::<String>();
        let _ = writeln!(output, "{label} │{cells}");
    }
    let _ = writeln!(output, "     └{}", "─".repeat(steps.len()));
    let time = |value: &Value| {
        value
            .as_u64()
            .and_then(|at| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at as i64))
            .map_or_else(|| "?".to_owned(), |at| at.format("%m-%d %H:%M").to_string())
    };
    let (from, to) = (time(&history["since_ms"]), time(&history["until_ms"]));
    let gap = (steps.len() + 1).saturating_sub(from.chars().count() + to.chars().count() + 4).max(1);
    let _ = writeln!(output, "      {from} UTC{}{to}", " ".repeat(gap));
    let low = known.iter().copied().fold(f64::INFINITY, f64::min);
    let now = steps.iter().rev().flatten().next().copied().unwrap_or_default();
    let _ = writeln!(output, "LOW {low:.0}% · LATEST {now:.0}% · {readings} readings · a column is its lowest reading");
    output
}

fn render_slo_explanation(row: &Value) -> String {
    use std::fmt::Write as _;

    let text = |field: &str| row[field].as_str().unwrap_or_default();
    let mut output = String::new();
    let _ = writeln!(output, "{}  {} · {}", text("id"), text("kind"), text("status"));
    let _ = writeln!(output, "TARGET   {}", text("target"));
    let _ = writeln!(output, "CURRENT  {} · {}", text("current"), text("window"));
    if let Some(left) = row["remaining_percent"].as_f64() {
        let _ = writeln!(output, "LEFT     {left:.0}% of the {}", if row["kind"] == "budget" { "allowance" } else { "error budget" });
    }
    if let Some(at) = row["resets_at_unix_ms"]
        .as_u64()
        .and_then(|at| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at as i64))
    {
        let _ = writeln!(output, "RESETS   {}", at.format("%Y-%m-%d %H:%M UTC"));
    }
    let _ = writeln!(output, "\n{}", text("explain"));
    output
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
    if let Some(estimate) = report.get("agent_messages") {
        output.push_str("\nAGENT MESSAGES · daily estimate · UTC · API-equivalent\n");
        let _ = writeln!(
            output,
            "Count × recipient allowance; includes useful work, not measured waste."
        );
        let _ = writeln!(
            output,
            "Fleet fallback ${:.2}–${:.2}/message · {}",
            estimate["fallback_low_microusd"].as_u64().unwrap_or(0) as f64 / 1_000_000.0,
            estimate["fallback_high_microusd"].as_u64().unwrap_or(0) as f64 / 1_000_000.0,
            estimate["source"].as_str().unwrap_or("fleet fallback")
        );
        output.push_str("DATE  MESSAGES  ESTIMATE  SHARE OF PRICED USAGE  CALIBRATED\n");
        for day in estimate["days"].as_array().into_iter().flatten() {
            let date = day["day_start_ms"]
                .as_u64()
                .and_then(|at| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at as i64))
                .map_or_else(|| "?".into(), |at| at.format("%Y-%m-%d").to_string());
            let partial = day["until_ms"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(day["since_ms"].as_u64().unwrap_or(0))
                < 86_400_000;
            let share = match (day["low_percent"].as_f64(), day["high_percent"].as_f64()) {
                (Some(low), Some(high)) => format!(
                    "{low:.1}–{high:.1}%{}",
                    if day["unpriced_tokens"].as_u64().unwrap_or(0) > 0 {
                        "*"
                    } else {
                        ""
                    }
                ),
                _ => "unknown (no priced usage)".into(),
            };
            let _ = writeln!(
                output,
                "{date}{}  {}  ${:.2}–${:.2}  {share}  {}/{}",
                if partial { " (partial)" } else { "" },
                day["messages"],
                day["low_microusd"].as_u64().unwrap_or(0) as f64 / 1_000_000.0,
                day["high_microusd"].as_u64().unwrap_or(0) as f64 / 1_000_000.0,
                day["calibrated_messages"],
                day["messages"]
            );
        }
        output.push_str("* Unpriced tokens are excluded from the denominator. Latest 31 UTC days; edge days are clipped.\n");
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
            "LIMITS  {} · the highest recent weekly reading of each account",
            limits.len()
        );
        let _ = writeln!(output, "WEEKLY  5-HOUR  WEEKLY RESET  MEASURED  account");
        for limit in limits {
            let _ = writeln!(
                output,
                "{}  {}  {}  {}  {}{}",
                percent(&limit["weekly_percent"]),
                percent(&limit["five_hour_percent"]),
                time(&limit["weekly_resets_at_unix_ms"]),
                time(&limit["measured_at_unix_ms"]),
                account_name(&limit["account"], &limit["account_ref"]),
                if limit["exempt"].as_bool() == Some(true) {
                    "  exempt"
                } else {
                    ""
                },
            );
            if limit["stale"].as_bool() == Some(true) {
                output.truncate(output.trim_end().len());
                output.push_str("  stale\n");
            }
        }
        let names = |key: &str| {
            report["limits_policy"][key]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (accounts, harnesses) = (names("exempt_accounts"), names("exempt_harnesses"));
        if !accounts.is_empty() || !harnesses.is_empty() {
            let _ = writeln!(
                output,
                "EXEMPT  never stopped at {}% · accounts: {} · harnesses: {}",
                report["limits_policy"]["stop_at_weekly_percent"],
                if accounts.is_empty() { "none" } else { &accounts },
                if harnesses.is_empty() { "none" } else { &harnesses },
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

async fn run_subject(client: &Client, command: SubjectCommand, json_output: bool) -> Result<()> {
    match command {
        SubjectCommand::Reply {
            subject,
            registration,
            revision,
            episode,
            fields_file,
            actor,
            idempotency_key,
        } => {
            reject_foreign_agent_actor(&actor)?;
            let fields = serde_json::from_slice(&std::fs::read(fields_file)?)?;
            let result: Value = client
                .post(
                    "/v1/custom/reply",
                    &st3::store::custom::ReplyRequest {
                        subject,
                        registration,
                        revision,
                        episode,
                        fields,
                        actor,
                        idempotency_key,
                    },
                )
                .await?;
            print_value(&result, json_output)
        }
        SubjectCommand::Basis { subject, kinds } => {
            let result: Value = client
                .get(&format!(
                    "/v1/custom/basis?subject={}&kinds={}",
                    urlencoding::encode(&subject),
                    urlencoding::encode(&kinds.join(","))
                ))
                .await?;
            print_value(&result, json_output)
        }
        SubjectCommand::Show(args) => {
            if args.subject.starts_with("custom/") && !args.kdl
                && let Ok(result) = client
                    .get::<Value>(&format!(
                        "/v1/client/custom-subjects/{}",
                        urlencoding::encode(&args.subject)
                    ))
                    .await
            {
                return print_value(&result, json_output);
            }
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

#[derive(serde::Deserialize)]
struct LocalEventPage {
    items: Vec<EventRecord>,
    next_after: Option<u64>,
}

#[derive(Debug)]
struct EventCursorGap(String);

impl std::fmt::Display for EventCursorGap {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(&self.0)
    }
}
impl std::error::Error for EventCursorGap {}

fn event_cursor_error(error: anyhow::Error, query: &str, trace: bool) -> anyhow::Error {
    let Some((410, "cursor-gap", _, details)) = st3::client::api_error_parts(&error) else {
        return error;
    };
    let Some(floor) = details.get("resume_floor").and_then(Value::as_u64) else {
        return error;
    };
    let Some(frontier) = details.get("frontier").and_then(Value::as_u64) else {
        return error;
    };
    let mut filters = String::new();
    let mut after = "unknown";
    for (key, value) in query.split('&').filter_map(|pair| pair.split_once('=')) {
        match key {
            "after" => after = value,
            "subject" | "owner_run" => {
                let value = urlencoding::decode(value).unwrap_or_else(|_| value.into());
                let quoted = format!("'{}'", value.replace('\'', "'\"'\"'"));
                if key == "owner_run" {
                    filters.push_str(" --owner-run");
                }
                filters.push(' ');
                filters.push_str(&quoted);
            }
            _ => {}
        }
    }
    let mut message = format!(
        "event cursor {after} has a gap: retained floor {floor}, frontier {frontier}. Missed transitions were not replayed."
    );
    if trace {
        message.push_str(&format!(" Inspect retained claims, then explicitly continue on the same endpoint with `st trace show{filters} --after-index {frontier} --follow`"));
    } else {
        message.push_str(" Inspect the current subject state and retry this command explicitly.");
    }
    EventCursorGap(message).into()
}

#[derive(Default)]
struct LocalEventFeed {
    trace: bool,
}

impl LocalEventFeed {
    async fn read(&mut self, client: &Client, query: &str) -> Result<LocalEventPage> {
        client
            .get(&format!("/v1/events/page?{query}"))
            .await
            .map_err(|error| {
                if st3::client::is_missing_route(&error) {
                    anyhow::anyhow!(
                        "this daemon does not support bounded event continuation, required by this CLI from 2026-10-16 13:50 UTC. If the daemon just restarted, rerun the original command first. Otherwise, install rolling edge on the daemon host with `curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/compoundingtech/smalltalk/main/install.sh | sh -s -- --edge --no-run`; for a managed daemon run `st service restart`, then rerun the original command"
                    )
                } else {
                    event_cursor_error(error, query, self.trace)
                }
            })
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
    let mut event_feed = LocalEventFeed::default();
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
        let page = event_feed
            .read(
                client,
                &format!("after={cursor}{scope}&wait=true&timeout_ms=30000"),
            )
            .await?;
        if let Some(scanned) = page.next_after {
            cursor = cursor.max(scanned);
        }
    }
}

async fn agent_wait_interruption(client: &Client, actor: &str) -> Result<Option<String>> {
    let work: Vec<StepRunView> = client
        .get(&format!(
            "/v1/work?actor={}&include_terminal=false&caller=wait-interruption",
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

async fn doctor_request<T: serde::de::DeserializeOwned>(
    client: &Client,
    path: &str,
    readiness: Option<&st3::startup::Readiness>,
    json_output: bool,
) -> Result<T> {
    match client.get(path).await {
        Err(error)
            if st3::client::daemon_unreachable(&error).is_some()
                && readiness.is_some_and(|startup| startup.status == "starting") =>
        {
            let startup = readiness.expect("starting observation was checked");
            if json_output {
                print_value(
                    &serde_json::json!({"status":"fail", "startup":startup,
                    "checks":[{"status":"fail", "name":"startup", "message":startup.summary()}]}),
                    true,
                )?;
            } else {
                println!("fail\tstartup\t{}", startup.summary());
            }
            anyhow::bail!("the daemon is starting; the API is not ready");
        }
        Err(error) if st3::client::daemon_unreachable(&error).is_some() => {
            if json_output {
                let message = doctor_cli::human_outage_message(&error)
                    .unwrap_or_else(|| plain_error(&error));
                print_value(&json!({"status": "fail", "checks": [{
                    "name": "daemon", "status": "fail", "message": message
                }]}), true)?;
            }
            Err(error)
        }
        outcome => outcome,
    }
}

async fn run_doctor(
    client: &Client,
    args: DoctorArgs,
    json_output: bool,
    unknown_config_keys: &[String],
) -> Result<()> {
    let config_check = doctor_cli::client_config_check(unknown_config_keys);
    if args.developer {
        let environment = st3::environment::snapshot()?;
        let check = doctor_cli::developer_tools(&environment);
        let status = doctor_cli::client_config_status(&check.status, config_check.is_some());
        if json_output {
            print_value(
                &json!({"status": status, "scope": "local developer tools", "checks":
                    std::iter::once(&check).chain(config_check.iter()).collect::<Vec<_>>()}),
                true,
            )?;
        } else {
            println!("{}\t{}\t{}", check.status, check.name, check.message);
            if let Some(config_check) = &config_check {
                println!(
                    "{}\t{}\t{}",
                    config_check.status, config_check.name, config_check.message
                );
            }
        }
        anyhow::ensure!(status != "fail", "st doctor found a failed check");
        anyhow::ensure!(
            !args.strict || status != "warn",
            "st doctor found a computed warning in strict mode"
        );
        return Ok(());
    }
    let readiness = client.socket_path().and_then(st3::startup::read);

    if args.performance {
        let mut report: Value =
            doctor_request(client, "/v1/performance", readiness.as_ref(), json_output).await?;
        if json_output {
            if let Some(config_check) = &config_check {
                report["client_config"] = serde_json::to_value(config_check)?;
            }
            if let Some(startup) = &readiness {
                report["startup"] = serde_json::to_value(startup)?;
            }
            return print_value(&report, true);
        }
        if let Some(startup) = &readiness {
            let status = if startup.status == "starting" {
                "info"
            } else {
                "pass"
            };
            println!("{status}\tstartup\t{}", startup.summary());
        }
        if let Some(config_check) = &config_check {
            println!(
                "{}\t{}\t{}",
                config_check.status, config_check.name, config_check.message
            );
        }
        print!("{}", render_performance(&report));
        return Ok(());
    }
    let mut report: DoctorReport =
        doctor_request(client, "/v1/doctor", readiness.as_ref(), json_output).await?;
    if let Some(config_check) = config_check {
        report.checks.push(config_check);
        if report.status == "pass" {
            report.status = "warn".into();
        }
    }
    if json_output {
        let mut value = serde_json::to_value(&report)?;
        if let Some(startup) = &readiness {
            value["startup"] = serde_json::to_value(startup)?;
        }
        print_value(&value, true)?;
    } else {
        if let Some(version) = &report.machine_version {
            println!("daemon\t{version}");
        }
        if let Some(startup) = &readiness {
            let status = if startup.status == "starting" {
                "info"
            } else {
                "pass"
            };
            println!("{status}\tstartup\t{}", startup.summary());
        }
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

/// Envelopes this node admitted beyond a writer's high water, which the fleet fenced, or None.
fn render_fenced_admissions(residue: &[smallclaims::store::FleetAdmissionResidue]) -> Option<String> {
    let fenced = residue
        .iter()
        .filter(|item| item.reason == "admitted-beyond-high-water")
        .map(|item| format!("{} from {}", envelope_count(item.envelopes), item.writer))
        .collect::<Vec<_>>();
    (!fenced.is_empty()).then(|| {
        format!(
            "this node admitted {} beyond the writer's high water, which the fleet fenced, so its \
             claims differ from the fleet's; st doctor reports them",
            fenced.join(", ")
        )
    })
}

/// This node's first sync in one line.
fn render_first_sync(first: &st3::model::ReplicationFirstSync, now: u128) -> String {
    let peer = first.peer.as_deref().unwrap_or("its peer");
    let when = first
        .ended_at_unix_ms
        .map(|at| relative_time(at, now))
        .unwrap_or_default();
    match first.state.as_str() {
        "verified" if first.authority_digest.is_some() => format!(
            "first sync verified {when}: this node then held the same {} as {peer}; log digest {}",
            envelope_count(first.envelopes.unwrap_or(0)),
            short_digest(first.authority_digest.as_deref().unwrap_or("unknown")),
        ),
        "verified" => format!(
            "first sync verified {when}: this node then held the same {} as {peer}",
            envelope_count(first.envelopes.unwrap_or(0)),
        ),
        "failed" => format!(
            "first sync failed {when} on an earlier build: {}; it ends at the next exchange where this node holds the same claims as {peer}",
            first.message.as_deref().unwrap_or("no reason recorded")
        ),
        _ => format!(
            "first sync from {peer} since {}",
            relative_time(first.started_at_unix_ms, now)
        ),
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
            "sync\tcatching up: {} has {} this node lacks, {} (arrival only)",
            peer.peer,
            envelope_count(sync.peer_only_envelopes),
            arrival_estimate(sync)
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
        for (label, record) in [
            ("last successful exchange", &peer.last_successful_exchange),
            ("last failed exchange", &peer.last_failed_exchange),
        ] {
            if let Some(record) = record {
                let _ = writeln!(
                    output,
                    "  {label} {} via {}{}",
                    relative_time(record.at_unix_ms, now),
                    record.carrier.as_str(),
                    record
                        .fabric_route
                        .as_ref()
                        .map(|route| format!(" (Fabric route {route})"))
                        .unwrap_or_default()
                );
            }
        }
        if let Some(worker) = &peer.worker {
            let _ = writeln!(
                output,
                "  worker {}: last attempt {}",
                worker.phase,
                relative_time(worker.last_attempt_at_unix_ms, now)
            );
            if let Some(at) = worker.next_retry_at_unix_ms {
                let _ = writeln!(
                    output,
                    "  next retry in {}s (unix ms {at})",
                    at.saturating_sub(now).div_ceil(1000)
                );
            }
        }
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
            // A stale view carries no forecast, not even "caught up": say only what was true.
            if sync.peer_only_envelopes == 0 {
                let _ = writeln!(
                    output,
                    "  nothing was missing at the last measurement (arrival only; measured {})",
                    relative_time(sync.measured_at_unix_ms, now)
                );
            }
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
                    "  exchanges cannot fix this; one node projects the same claims wrongly, and its views are wrong until that is fixed"
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
                    "  same envelopes (measured {}); projection comparison waits for a settled exchange or a compatible build",
                    relative_time(sync.measured_at_unix_ms, now)
                );
                if let Some(reason) = peer.projection_comparison_waiting_reason.as_deref() {
                    let _ = writeln!(output, "  waiting because {reason}");
                }
            } else if peer_graphs_equal(peer, local_graph_digest) {
                let _ = writeln!(
                    output,
                    "  in sync: the same envelopes and the same graph (measured {}, graphs compared {})",
                    relative_time(sync.measured_at_unix_ms, now),
                    compared_ago(peer, now)
                );
            } else {
                let _ = writeln!(
                    output,
                    "  same envelopes (measured {}), but the graphs differ ({digests}; compared {}); the next exchange compares them",
                    relative_time(sync.measured_at_unix_ms, now),
                    compared_ago(peer, now)
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
            let closing = sync
                .closing_rate_per_second()
                .filter(|rate| *rate > 0.0)
                .map(|rate| format!("closing at {rate:.1}/s, "))
                .unwrap_or_default();
            let _ = writeln!(
                output,
                "  {rate}{closing}{} (arrival only; measured {})",
                arrival_estimate(sync),
                relative_time(sync.measured_at_unix_ms, now)
            );
        }
    }
    output
}

/// When the envelopes this node lacks will have arrived, or that the gap is not closing. It
/// forecasts arrival only: admission, projection and visibility can lag behind it.
fn arrival_estimate(sync: &st3::model::ReplicationPeerSync) -> String {
    // The lower of the catch-up and receive rates: nothing arriving is not catching up either.
    if sync.peer_only_envelopes != 0
        && sync.closing_rate_per_second().is_some_and(|rate| rate <= 0.0)
    {
        return "not catching up".into();
    }
    catch_up_estimate(sync.estimated_catch_up_seconds)
}

/// Whether the comparison the status reports for a peer found its projections equal. An older
/// daemon reports no result, and its digests are compared as they were before.
fn peer_graphs_equal(peer: &ReplicationPeerStatus, local_graph_digest: &str) -> bool {
    peer.projection_graphs_equal
        .unwrap_or_else(|| peer.graph_digest.as_deref() == Some(local_graph_digest))
}

/// When the reported comparison was made, so one held from an earlier exchange is never
/// presented as current.
fn compared_ago(peer: &ReplicationPeerStatus, now: u128) -> String {
    peer.projection_compared_at_unix_ms
        .or(peer.sync.as_ref().and_then(|sync| sync.graph_compared_at_unix_ms))
        .map(|at| relative_time(at, now))
        .unwrap_or_else(|| "at the last exchange".into())
}

/// The first 12 characters of a digest, enough to tell two apart in a status line.
fn short_digest(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
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
    output.push_str("last daemon start (advertised build)\n");
    for member in &status.participants {
        output.push_str(&format!(
            "  machine_version  {member}: {}\n",
            status.machine_version.get(member).map(String::as_str).unwrap_or("not advertised")
        ));
    }
    if !status.excused.is_empty() {
        output.push_str(&format!("excused       {}\n", names(&status.excused)));
    }
    if !status.left.is_empty() {
        output.push_str(&format!("left          {}\n", names(&status.left)));
    }
    if let Some(pending) = &status.pending {
        output.push_str(&format!("pending       {}\n", pending.checkpoint));
        for (member, build) in &pending.builds {
            output.push_str(&format!("  seal build  {member}: {build}\n"));
        }
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

fn run_service(command: ServiceCommand, json_output: bool) -> Result<()> {
    match command {
        ServiceCommand::Install { config } => {
            st3::service::install_from_config_path(
                Config::load_with_fleet(config.as_deref())?,
                config.as_deref(),
            )
        }
        ServiceCommand::Status => {
            let config = Config::load_unvalidated(None)?;
            let mut report = st3::service::status()?;
            if let Some(startup) = st3::startup::read(&config.client_socket()) {
                for service in &mut report.services {
                    if service.name == "st3.service" || service.name == "com.compoundingtech.st3" {
                        service.state = format!("{} · {}", service.state, startup.summary());
                    }
                }
            }
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

/// Resolves against all subjects of the kind. Destructive verbs use exact matching only;
/// navigation permits the fuzzy ladder. Unreachable lists retain the literal interpretation.
async fn resolve_member_subject(
    endpoint: &Endpoint,
    subject: &str,
    namespace: &str,
    entity: Entity,
    matching: completion::Matching,
) -> Result<String> {
    let literal = normalize_member_subject(subject, namespace);
    if subject.contains('/') {
        return Ok(literal);
    }
    let Endpoint::Unix(socket) = endpoint else {
        return Ok(literal);
    };
    let discovered = completion::LocalTarget::discover(&[]);
    let target = completion::LocalTarget {
        socket: socket.clone(),
        caller: discovered.as_ref().and_then(|target| target.caller.clone()),
        person: discovered.and_then(|target| target.person),
    };
    let listed = tokio::time::timeout(
        completion::RESOLVE_DEADLINE,
        completion::resolution_candidates(&target, entity),
    )
    .await;
    let Ok(Ok(candidates)) = listed else {
        return Ok(literal);
    };
    match completion::resolve(subject, namespace, &candidates, matching) {
        completion::Resolution::Subject(subject) => Ok(subject),
        completion::Resolution::Unmatched => Ok(literal),
        completion::Resolution::Ambiguous(matches) => {
            let listing = matches
                .iter()
                .map(|candidate| format!("  {}  {}", candidate.subject, candidate.description))
                .collect::<Vec<_>>()
                .join("\n");
            eprintln!(
                "st: `{subject}` matches {} subjects; name one exactly:\n{listing}",
                matches.len()
            );
            Err(CommandExit(2).into())
        }
    }
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
        .or_else(|| {
            std::env::var("ST_AGENT")
                .ok()
                .filter(|agent| !agent.is_empty())
        })
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
    // A named type uploads the bytes exactly; otherwise an SVG, a BMP or an image over the limit
    // is converted or scaled first, so read up to what scaling can still bring under it.
    let largest = if media_type.is_some() {
        st3::blobs::MAX_BLOB_BYTES as u64
    } else {
        st3::attach_image::MAX_INPUT_BYTES
    };
    anyhow::ensure!(
        metadata.len() <= largest,
        "{} is larger than the {} MiB an attachment may be",
        file.display(),
        largest / (1024 * 1024)
    );
    let bytes = fs::read(file).with_context(|| format!("read attachment {}", file.display()))?;
    let file_name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let (bytes, media_type, name) = match media_type {
        Some(media_type) => (bytes, media_type.to_owned(), file_name),
        None => {
            let prepared = st3::attach_image::prepare(file, bytes)?;
            if let Some(note) = &prepared.note {
                eprintln!("{note}");
            }
            (prepared.bytes, prepared.media_type.to_owned(), prepared.name)
        }
    };
    let uploaded = generated_client(endpoint, Some(actor))?
        .upload_blob(bytes, &media_type)
        .await
        .map_err(|error| anyhow::anyhow!("{}", error.plain()))?
        .value;
    Ok(st3::model::AttachmentInput {
        blob: uploaded.blob,
        media_type: uploaded.media_type,
        name,
    })
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
                    cli_output::stdout().write_all(&bytes)?;
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

/// A document as terminal markdown, or `None` when it is not text (a binary, or JSON and KDL
/// that read better as they are), so it prints as stored.
fn render_document(bytes: &[u8], width: usize) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let start = text.trim_start();
    if text.contains('\0') || start.starts_with('{') || start.starts_with('[') || start.starts_with("version ") {
        return None;
    }
    let theme = st3_conversation_ui::Theme::default();
    let lines = st3_conversation_ui::text::markdown(
        text,
        width.saturating_sub(2),
        theme.text(),
        &theme,
    );
    Some(
        lines
            .iter()
            .map(|line| format!("  {}", st3_conversation_ui::text::ansi(line)))
            .collect::<Vec<_>>()
            .join("\n"),
    )
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
        DocCommand::Get {
            reference,
            output,
            raw,
        } => {
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
            } else if let Some(rendered) = (!raw && cli_output::stdout().is_terminal())
                .then(|| render_document(&bytes, terminal_columns().unwrap_or(100).clamp(20, 140)))
                .flatten()
            {
                println!("{rendered}");
            } else {
                use std::io::Write as _;
                cli_output::stdout().write_all(&bytes)?;
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
        repo: args.repo.as_ref().map(|path| path.display().to_string()),
        base: args.base.clone(),
        branch: args.branch.clone(),
        remove_at_run_end: args.remove_at_run_end.then_some(true),
    };
    let key = args
        .message
        .as_ref()
        .map(|_| uuid::Uuid::now_v7().to_string());
    st3::creation::agent_document(&parameters, workspace, create_workspace, key.as_deref())
}

fn seat_subject(value: &str) -> String {
    if value.starts_with("agent/") {
        value.to_owned()
    } else {
        format!("agent/{value}")
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
    match agent.down_since.as_deref().filter(|_| agent.state == "down") {
        Some(since) => {
            let _ = writeln!(output, "STATE        down · no session since {since}");
        }
        None => {
            let _ = writeln!(output, "STATE        {}", agent.state);
        }
    }
    let _ = writeln!(output, "REACHABILITY {}", agent.reachability);
    let _ = writeln!(
        output,
        "HARNESS      {} · {}",
        agent.driver.as_deref().unwrap_or("none"),
        agent.harness_state.as_deref().unwrap_or("unobserved")
    );
    if agent.blocked_on.as_deref() == Some("human")
        && agent.ask.as_deref() == Some("permission")
    {
        let _ = writeln!(output, "AWAITING     approval");
    }
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
    if let Some(handoff) = &agent.handoff {
        let pending = if handoff.pending_sources.is_empty() { String::new() }
            else { format!(" · waiting for {}", handoff.pending_sources.join(", ")) };
        let _ = writeln!(output, "HANDOFF      {} → {}{pending}", handoff.phase, handoff.destination);
        if !handoff.overridden_sources.is_empty() {
            let _ = writeln!(output, "SOURCE OFFLINE {} · override recorded", handoff.overridden_sources.join(", "));
        }
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
    if let Some(rollout) = &agent.rollout {
        let phase = rollout["phase"].as_str().unwrap_or("unknown");
        let id = rollout["id"].as_str().unwrap_or("unknown");
        let forced = if rollout["forced"].as_bool() == Some(true) {
            " · forced"
        } else {
            ""
        };
        if rollout["mode"] == "manual" && phase == "pending" {
            let _ = writeln!(output, "ROLLOUT      published, rollout pending (manual)");
            let _ = writeln!(
                output,
                "CUTOVER      st agents rollout {} --as ACTOR",
                agent.header.id
            );
        } else {
            let _ = writeln!(output, "ROLLOUT      {phase}{forced} · {id}");
        }
        if let Some(blockers) = rollout["blocking"]
            .as_array()
            .filter(|items| !items.is_empty())
        {
            let _ = writeln!(
                output,
                "BLOCKERS     {}",
                blockers
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if let Some(reason) = rollout["reason"].as_str() {
            let _ = writeln!(output, "ROLLOUT WHY  {reason}");
        }
        if matches!(phase, "held" | "failed" | "blocked") {
            let _ = writeln!(
                output,
                "RETRY        st agents rollout {} --as ACTOR",
                agent.header.id
            );
        }
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
    if let Some(operation) = agent.extra.get("runtime_start") {
        let phase = operation["phase"].as_str().unwrap_or("unknown");
        let id = operation["operation_id"].as_str().unwrap_or("unknown");
        let running = operation["worker_running"] == true;
        let _ = writeln!(output, "START        {phase} · {id}{}",
            if running && phase == "revoked" { " · launch worker still running" } else { "" });
        let _ = writeln!(output, "START JOBS   {} running · {} revoked and still running",
            operation["running_jobs"].as_u64().unwrap_or(0),
            operation["revoked_running_jobs"].as_u64().unwrap_or(0));
        let _ = writeln!(output, "START OPEN   {} unresolved · {} revoked and unresolved",
            operation["unresolved_jobs"].as_u64().unwrap_or(0),
            operation["revoked_unresolved_jobs"].as_u64().unwrap_or(0));
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
        let held_down = agent
            .current_work
            .iter()
            .find(|label| label.id == *current)
            .and_then(|label| label.holder_down_since.as_deref())
            .map(|since| format!(" · held by a seat with no session since {since}"))
            .unwrap_or_default();
        let _ = writeln!(
            output,
            "CURRENT STEP {} · {}{held_down}",
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
                // A status that is neither work nor a wait, such as compacting.
                if let Some(activity) = &agent.activity {
                    let _ = writeln!(output, "  {activity}");
                }
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
    match &command {
        SchemaCommand::Register { file, actor } => {
            reject_foreign_agent_actor(actor)?;
            let manifest = serde_json::from_slice(&std::fs::read(file)?)?;
            let value: Value = client
                .post(
                    "/v1/schema/registrations",
                    &st3::store::custom::RegistrationRequest {
                        manifest,
                        actor: actor.clone(),
                    },
                )
                .await?;
            return print_value(&value, json_output);
        }
        SchemaCommand::Registrations => {
            let value: Value = client.get("/v1/schema/registrations").await?;
            return print_value(&value, json_output);
        }
        SchemaCommand::Registration { kind } => {
            let (name, hash) = kind
                .rsplit_once('@')
                .context("registration needs KIND@HASH")?;
            let value: Value = client.get("/v1/schema/registrations").await?;
            let item = value["items"]
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .find(|v| v["manifest"]["kind"] == name && v["registration"] == hash)
                })
                .context("registration is unavailable")?;
            return print_value(item, json_output);
        }
        _ => {}
    }
    let value: Value = client.get("/v1/schema").await?;
    let selected = match command {
        SchemaCommand::Register { .. }
        | SchemaCommand::Registrations
        | SchemaCommand::Registration { .. } => unreachable!("handled above"),
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

const STRUCTURED_REQUEST_HELP: &str = r##"A structured request as a JSON file (`-` reads stdin).

A `decision` proposes one action: exactly one `accept` answer, one `decline` answer and at most
one `request_changes` answer, each naming what happens next. A `choice` names 2 to 5 options,
and `"custom": true` also takes the person's own words. `feedback` asks for text and has no
answers. `why_person` says why no runtime fact or standing instruction settles it. Omit
`recommendation` to make none. An `update` asks nothing: it names `about`, the person's own
run or step or their message to you, and clears once read (`st work update` builds one). Subject kinds: pull_request, issue, document, mission, run,
step, agent, host, commit, link; `revision` pins what was reviewed. `--attach FILE` shows the
person an image with the request (up to 4), as `st conversations send --attach` does.

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
    if let Some(incarnation) = &work.submitted_incarnation {
        let _ = writeln!(output, "Submitted incarnation: {incarnation}");
    }
    if let Some(nudged) = work
        .nudged_at_unix_ms
        .and_then(|at| chrono::DateTime::from_timestamp_millis(i64::try_from(at).ok()?))
    {
        let at = nudged.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let _ = writeln!(
            output,
            "Nudged: {at} ({}), idle while holding it with nothing set to wake it",
            ago(&at, now_ms())
        );
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

async fn current_local_agent_incarnation(client: &Client, actor: &str) -> Result<Option<String>> {
    let status: StatusResponse = client
        .get(&format!(
            "/v1/status?subject={}",
            urlencoding::encode(actor)
        ))
        .await?;
    let binding = status
        .subjects
        .first()
        .and_then(|s| s.desired.as_ref())
        .map(st3::terminal_binding::from_desired)
        .transpose()?
        .flatten();
    let Some(binding) = binding else {
        return current_local_pty_incarnation(actor);
    };
    let Some(root) = std::env::var_os("PTY_ROOT").filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let observations = st_runtime::PtyRuntime::new(PathBuf::from(root)).snapshot()?;
    Ok(bound_pty_incarnation(&binding, &observations))
}

fn bound_pty_incarnation(
    binding: &st3::model::TerminalBinding,
    observations: &[st_runtime::PtyObservation],
) -> Option<String> {
    let matched = observations.iter().any(|o| {
        o.status == "running"
            && o.tags.get("st3.subject") == Some(&binding.subject)
            && o.pid
                .zip(o.created_at.as_deref())
                .is_some_and(|(pid, created)| format!("{pid}:{created}") == binding.incarnation)
    });
    matched.then(|| binding.agent_incarnation())
}

fn has_local_pty_registry() -> bool {
    std::env::var_os("PTY_ROOT").is_some_and(|value| !value.is_empty())
}

async fn wait_for_agent_incarnation_from(
    client: &Client,
    actor: &str,
    use_local_pty_registry: bool,
) -> Result<String> {
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut outage_logged = false;
    let mut retry = st3::client::DriverRetry::default();
    loop {
        // A restarted provider process can begin before the reconciler has projected its new PTY
        // observation. Reading the graph immediately would then bind this new driver to the old
        // incarnation forever. The local registry already contains the process executing us and
        // is the exact source from which the reconciler will derive the graph incarnation.
        let lookup = if use_local_pty_registry {
            current_local_agent_incarnation(client, actor).await
        } else {
            current_agent_incarnation(client, actor).await
        };
        match lookup {
            Ok(Some(incarnation)) => return Ok(incarnation),
            Ok(None) => retry.succeeded(),
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
                tokio::time::sleep(retry.failed(&error)).await;
                continue;
            }
            Err(error) => return Err(error),
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

async fn send_message(
    client: &Client,
    args: MessageSendArgs,
    attachments: Vec<st3::model::AttachmentInput>,
) -> Result<Option<MessageSendReceipt>> {
    let id = uuid::Uuid::now_v7().simple().to_string();
    let mission_id = format!("message/{id}");
    // Before anything else: an agent that sends to a person needs this answer, not another.
    let to = normalize_message_subject(&args.to);
    st3::model::refuse_person_recipient(&to).map_err(|error| anyhow::anyhow!(error.message))?;
    reject_foreign_agent_actor(&args.from)?;
    let from = normalize_message_subject(&args.from);
    let mut args = args;
    if args.wake.kind == "wake" && args.tags.iter().any(|tag| tag == st3::silent::SILENT_TAG) {
        anyhow::bail!("wake messages cannot carry the silent delivery tag; use --kind silent");
    }
    for tag in args.wake.tags() {
        if !args.tags.contains(&tag) {
            args.tags.push(tag);
        }
    }
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
                    Ok(Some(receipt)) => {
                        warn_unconfirmed_silent(&request, &receipt);
                        return Ok(Some(receipt));
                    }
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
        Ok(receipt) => {
            warn_unconfirmed_silent(&request, &receipt);
            Ok(Some(receipt))
        }
        Err(error) => Err(message_send_error(error, &request.idempotency_key)),
    }
}

fn warn_unconfirmed_silent(request: &MessageSendRequest, receipt: &MessageSendReceipt) {
    if request.from.starts_with("agent/")
        && request
            .tags
            .iter()
            .any(|tag| tag == st3::silent::SILENT_TAG)
        && receipt.kind.as_deref() != Some("silent")
    {
        eprintln!(
            "st: daemon did not confirm silent holding; this message may wake its recipient. Upgrade the sender and recipient owner daemons."
        );
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

/// Who signed a message a person wrote, as the daemon read it: `None` for an agent's message or a
/// daemon that does not say.
async fn message_provenance(client: &Client, reference: &str) -> Option<Value> {
    let reference = normalize_message_reference(reference);
    let read: Value = client
        .get(&format!(
            "/v1/messages/read/{}",
            urlencoding::encode(&reference)
        ))
        .await
        .ok()?;
    read.get("provenance").cloned()
}

/// A provenance as one line: `verified · person/example · example phone (secure enclave) · p256:BPLX…`.
fn provenance_line(provenance: &Value) -> String {
    let text = |field: &str| provenance.get(field).and_then(Value::as_str);
    let mut parts = vec![text("verdict").unwrap_or("unknown").to_owned()];
    if let Some(reason) = text("reason") {
        parts.push(reason.to_owned());
    }
    for field in ["signer", "device"] {
        if let Some(value) = text(field) {
            parts.push(value.to_owned());
        }
    }
    if let Some(key) = text("key") {
        let shown: String = key.chars().take(16).collect();
        parts.push(format!("{shown}…"));
    }
    parts.join(" · ")
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
                delegation: None,
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
                delegation: None,
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
                delegation: None,
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
                delegation: None,
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

struct TerminalScreen;

impl TerminalScreen {
    fn open() -> Result<Self> {
        print!("\x1b[?25l");
        cli_output::stdout().flush()?;
        Ok(Self)
    }
}

impl Drop for TerminalScreen {
    fn drop(&mut self) {
        print!("\x1b[?25h");
        let _ = cli_output::stdout().flush();
    }
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

#[cfg(test)]
mod native_seed_driver_tests;

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
    #[serde(default)]
    claude_attachment_phase: String,
    #[serde(default)]
    claude_attachment_episode: u64,
    #[serde(default)]
    claude_attachment_pending: Option<PendingClaudeAttachment>,
    // An acknowledged phase survives graceful re-exec. Legacy resume files lack this
    // flag and require one corrective publication because their last POST was uncertain.
    #[serde(default)]
    claude_attachment_reconciled: bool,
    #[serde(default)]
    claude_attachment_terminal: Option<ClaudeAttachmentTerminal>,
    #[serde(default)]
    claude_readiness_fallback: ClaudeReadinessFallback,
    /// This image's own settle. A replacement image draws a new one, so it is never carried.
    #[serde(skip)]
    settle: ResumeSettle,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PendingClaudeAttachment {
    fence: st3::mailbox::Fence,
    phase: String,
    input: ClaimInput,
    input_digest: String,
}

// One terminal rejection inhibits publication for this binding, including re-exec.
// Readiness still reports; a replacement binding starts a new publication lifecycle.
#[derive(serde::Serialize, serde::Deserialize)]
struct ClaudeAttachmentTerminal {
    fence: st3::mailbox::Fence,
    reason: String,
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

/// When a driver may send the daemon what its seat's delivery does not need: its harness events,
/// Claude attachment checks, native session, harness publications, subagents and work renewals.
/// A deploy re-executes every driver in the seconds it restarts the daemon. A resumed driver waits
/// a random share of [`st3::client::RESUME_SETTLE`], so the drivers do not all reach the new
/// daemon's writer at once. Mail, receipts, delivery control and the provider's exit never wait.
#[derive(Clone, Copy, Debug, Default)]
struct ResumeSettle(Option<tokio::time::Instant>);

impl ResumeSettle {
    /// A launched driver has nothing to spread.
    fn none() -> Self {
        Self(None)
    }

    fn resumed() -> Self {
        Self::after(st3::client::resume_settle_pause())
    }

    fn after(pause: Duration) -> Self {
        Self(Some(tokio::time::Instant::now() + pause))
    }

    fn passed(&self) -> bool {
        self.0
            .is_none_or(|until| tokio::time::Instant::now() >= until)
    }

    async fn wait(&self) {
        if let Some(until) = self.0 {
            tokio::time::sleep_until(until).await;
        }
    }

    /// Delivery and local housekeeping keep their interval during settle. Do not poll the
    /// deferred control retry until it is eligible: even an overdue retry must not wake it early.
    /// The returned eligibility guards the report body and its success/failure state updates.
    async fn control_tick(
        &self,
        retry: &mut st3::client::DriverRetry,
        interval: &mut tokio::time::Interval,
    ) -> bool {
        if self.passed() {
            retry.tick(interval).await;
        } else {
            interval.tick().await;
        }
        self.passed()
    }

    /// Renewals do no interval or retry work before settle. On eligibility the existing due
    /// interval/retry can run once; Skip intervals prevent a catch-up burst. No state is reset.
    async fn retry_tick(
        &self,
        retry: &mut st3::client::DriverRetry,
        interval: &mut tokio::time::Interval,
    ) {
        self.wait().await;
        retry.tick(interval).await;
    }

    /// When the driver starts checking the channel's attachment, so a resumed driver's wait does
    /// not count toward the channel's 20 s to attach.
    fn attach_started(&self) -> Instant {
        let left = self.0.map_or(Duration::ZERO, |until| {
            until.saturating_duration_since(tokio::time::Instant::now())
        });
        Instant::now() + left
    }
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
    native_state_seen: bool,
    evidence_deadline: Option<serde_json::Value>,
    retry_pending: bool,
    initial_wake: bool,
    pipe: Option<tokio::io::unix::AsyncFd<std::fs::File>>,
    settle: ResumeSettle,
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
        // The provider token is separate from the runtime incarnation. Only the spool's
        // producer/runtime binding proves that a saved snapshot belongs to this launch.
        // Re-exec may restore it; a replacement runtime waits for its own provider event.
        let snapshot = if enabled {
            st_drivers::harness_events::read_runtime_state(dir, runtime)?
        } else {
            None
        };
        let provider_incarnation = snapshot.as_deref().and_then(|raw| {
            st_drivers::harness_state::read_raw_at(raw, None, st_drivers::message::now_ms())
                .evidence_incarnation
        });
        let native_state_seen = snapshot.as_deref().is_some_and(|raw| {
            let state = st_drivers::harness_state::read_raw_at(raw, None, st_drivers::message::now_ms());
            !(state.state == st_drivers::harness_state::Activity::Unknown
                && state.reason.as_deref() == Some("claimed"))
        });
        let evidence_deadline = snapshot
            .as_deref()
            .and_then(|raw| serde_json::from_slice(raw).ok());
        Ok(Self {
            dir: dir.into(),
            runtime: runtime.into(),
            enabled,
            provider_incarnation,
            native_state_seen,
            evidence_deadline,
            retry_pending: enabled,
            initial_wake: enabled,
            pipe,
            settle: ResumeSettle::none(),
        })
    }
    /// Hold every drain until `settle` passes. Hooks keep writing the outbox meanwhile; their
    /// wakes go to a non-blocking pipe, so a held wake never blocks a hook.
    fn settle(mut self, settle: ResumeSettle) -> Self {
        self.settle = settle;
        self
    }
    /// Whether a pending drain may go now.
    fn retry_due(&self) -> bool {
        self.retry_pending && self.settle.passed()
    }
    async fn recv(&mut self) -> Result<()> {
        if self.enabled && !self.settle.passed() {
            self.settle.wait().await;
            self.initial_wake = false;
            return Ok(());
        }
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
    async fn drain_live(
        &mut self,
        client: &Client,
        subject: &str,
        driver: &str,
        ready: &mut bool,
    ) -> Result<bool> {
        self.drain_events(client, subject, driver, ready, true).await
    }

    async fn drain(
        &mut self,
        client: &Client,
        subject: &str,
        driver: &str,
        ready: &mut bool,
    ) -> Result<()> {
        self.drain_events(client, subject, driver, ready, false)
            .await
            .map(|_| ())
    }

    async fn drain_events(
        &mut self,
        client: &Client,
        subject: &str,
        driver: &str,
        ready: &mut bool,
        wait_for_completion: bool,
    ) -> Result<bool> {
        if !self.enabled {
            return Ok(false);
        }
        self.retry_pending = true;
        // Bound a wake's work so a backlog does not hold back native delivery.
        let mut events = st_drivers::harness_events::pending(&self.dir, 64)?;
        for event in &mut events {
            // Account binding is outbox metadata, not part of the producer's observation.
            // Preserve it separately for the accounting claim builders below.
            let account_ref = event.payload.as_object_mut()
                .and_then(|fields| fields.remove("account_ref"));
            let publisher = ObservationClient {
                client,
                event: Some((&self.runtime, event.sequence, &self.dir, account_ref.as_ref().and_then(Value::as_str))),
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
                        // The wrapper writes its terminal receipt before its blocking task
                        // returns. Publishing it now would fence our mailbox before the task's
                        // actual success/failure can reach the normal exit-report path. Keep
                        // this event unacknowledged until that path drains it. Exitless hook
                        // observations and predecessor/foreign provider records still publish.
                        if wait_for_completion
                            && event.kind == "harness-state"
                            && observed.state == st_drivers::harness_state::Activity::Ended
                            && observed.exit.is_some()
                            && observed.evidence_incarnation.is_some()
                            && st_drivers::harness_events::read_runtime_state(&self.dir, &self.runtime)?
                                .is_some_and(|current| {
                                    let current = st_drivers::harness_state::read_raw_at(
                                        &current, None, event.queued_at_ms,
                                    );
                                    current.evidence_incarnation == observed.evidence_incarnation
                                        && current.ownership_sequence == observed.ownership_sequence
                                        && current.transition_sequence == observed.transition_sequence
                                        && current.exit == observed.exit
                                })
                        {
                            return Ok(true);
                        }
                        // Admission precedes the provider claim. Only a state event fenced to
                        // this runtime can expose its diagnostic; an old snapshot cannot fence
                        // a successor. Refused omp launches are handled on the exit path.
                        if matches!(driver, "omp" | "opencode") {
                            publish_admission_diagnostic(
                                client,
                                subject,
                                &self.runtime,
                                &self.dir,
                                &mut None,
                            )
                            .await?;
                        }
                        self.provider_incarnation = observed.evidence_incarnation.clone();
                        self.evidence_deadline =
                            (event.kind != "harness-state-expired").then(|| event.payload.clone());
                    }
                    let placeholder = observed.state
                        == st_drivers::harness_state::Activity::Unknown
                        && observed.reason.as_deref() == Some("claimed");
                    if !placeholder {
                        // Even an unknown, blocked or ended native state prevents a synthetic
                        // startup-ready claim. Mark before POST, including a failed publication.
                        if event.runtime_incarnation == self.runtime && source_driver == driver {
                            self.native_state_seen = true;
                        }
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
                    let claim = native_timeline_claim(client, subject,
                        &event.runtime_incarnation, operation).await?;
                    let _: ClaimRecord = publisher
                        .post(
                            "/v1/claims",
                            &claim,
                        )
                        .await?;
                }
                other => anyhow::bail!("unsupported harness event `{other}`"),
            }
            st_drivers::harness_events::acknowledge(&self.dir, event.sequence)?;
        }
        self.retry_pending = events.len() == 64;
        self.initial_wake = self.retry_pending;
        Ok(false)
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
                    .unwrap_or(if claim.kind == "harness.todo.observed" { "normalized" } else { "" })
            );
            let claim: ClaimInput = serde_json::from_value(st_drivers::harness_events::prepare_publication(
                dir,
                sequence,
                &slot,
                &serde_json::to_value(&claim)?,
            )?)?;
            // Diagnostics are retained graph facts, not local observation slots. Preserve
            // their episode key; /harness-events replaces keys with its observation slot.
            if claim.kind == "harness.diagnostic" {
                return self.client.post(path, &claim).await;
            }
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
        (
            "provider_auth".into(),
            observed
                .provider_auth
                .map(Value::Bool)
                .unwrap_or(Value::Null),
        ),
        (
            "provider_auth_sequence".into(),
            Value::from(observed.provider_auth_sequence),
        ),
        ("driver".into(), Value::String(driver.into())),
        ("transport".into(), Value::String(transport.into())),
        (
            "blocked_on".into(),
            Value::String(observed.blocked_on.as_str().into()),
        ),
        ("ask".into(), Value::String(observed.ask.as_str().into())),
        ("background_jobs".into(), observed.background_jobs.map(Value::from).unwrap_or(Value::Null)),
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
            Value::from(limits.observed_at_ms.unwrap_or(observed.observed_at_ms)),
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

fn timeline_record_is_current(
    record: &st_drivers::harness_timeline::Record,
    driver: &str,
    provider_incarnation: Option<&str>,
) -> bool {
    record.driver == driver && provider_incarnation == Some(record.incarnation_id.as_str())
}

/// Bind a reported failure to the current ready wake before promoting it to a diagnostic.
/// The CLI cannot report through the socket it was denied; the native driver can.
async fn native_timeline_claim(
    client: &Client, subject: &str, incarnation: &str,
    operation: st_drivers::harness_timeline::Operation,
) -> Result<ClaimInput> {
    let mut failure = None;
    if operation.entry_type == "error"
        && operation.body["code"] == st_drivers::work_claim_failure::CODE
        && let Ok(receipt) = serde_json::from_value::<st_drivers::work_claim_failure::Receipt>(operation.body["details"].clone())
        && receipt.valid() && receipt.actor == subject
    {
        let work: Vec<StepRunView> = client.get(&format!(
            "/v1/work?actor={}&caller=timeline-binding", urlencoding::encode(subject))).await?;
        if let Some(step) = work.into_iter().find(|step| {
            step.subject == receipt.step_run && step.status == "ready"
                && step.wake.as_ref().is_some_and(|wake| {
                    wake.incarnation_id == incarnation && wake.attempts > 0
                        && wake.last_attempt_at_unix_ms.is_some_and(|sent| receipt.follows_wake(sent, operation.observed_at_unix_ms))
                })
        }) {
            failure = Some((receipt, step));
        }
    }
    if let Some((receipt, step)) = failure {
        return Ok(ClaimInput {
            subject: subject.into(), kind: "harness.diagnostic".into(), actor: Some(subject.into()),
            fields: BTreeMap::from([
                ("code".into(), st_drivers::work_claim_failure::CODE.into()),
                ("status".into(), "failed".into()), ("severity".into(), "error".into()),
                ("reason".into(), receipt.reason().into()),
                ("incarnation_id".into(), incarnation.into()),
                ("step_run".into(), step.subject.clone().into()),
                ("attempt".into(), step.attempt.into()),
                ("readiness_epoch".into(), step.readiness_epoch.into()),
                ("matched_line".into(), serde_json::to_string(&receipt)?.into()),
                ("observed_since_ms".into(), receipt.observed_at_unix_ms.into()),
                ("driver".into(), operation.driver.into()),
            ]), evidence: Vec::new(), expected_subject: None,
            idempotency_key: Some(st_drivers::work_claim_failure::key(subject, &step.subject,
                step.attempt, step.readiness_epoch, incarnation)),
        });
    }
    let fields = timeline_claim_fields(operation, incarnation);
    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&fields)?));
    Ok(ClaimInput { subject: subject.into(), kind: "harness.timeline".into(),
        actor: Some(subject.into()), fields, evidence: Vec::new(), expected_subject: None,
        idempotency_key: Some(format!("harness-timeline:{subject}:{digest}")),
    })
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

/// A `harness.observed` claim's fields with the driver's quiescence report added.
fn with_quiescence(mut fields: BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    st3::suspension::annotate_quiescence(&mut fields);
    fields
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
        "content": st_drivers::ding::with_tag_notices(st_drivers::ding::st3_notification_with_attachments(
            &message.subject,
            &message.from,
            &message.to,
            message.title.as_deref(),
            body,
            &st_drivers::ding::st3_body_sha256(body),
            attachments,
        ), &message.tags.iter().filter(|tag| tag.as_str() != st3::silent::SILENT_TAG || st3::silent::is_held(message)).cloned().collect::<Vec<_>>()),
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
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
    subject: &str,
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
) -> Result<bool> {
    if let Ok(frame) = serde_json::from_str::<Value>(line) {
        let frame_type = frame.get("type").and_then(Value::as_str).unwrap_or("unknown");
        let handled = match frame_type {
            "state" | "session" | "ready" | "delivered" | "read" | "failed" | "todo" => true,
            "timeline" | "context" | "turn" | "subagent" => observer.is_some(),
            // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
            "delivery_ready" | "retry_pending_ask" | "diagnostic" => true,
            // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
            _ => false,
        };
        if !handled {
            tracing::debug!(
                frame_type,
                observer_enabled = observer.is_some(),
                "unhandled pi-family channel frame"
            );
        }
        if let Some(observer) = observer.as_mut() {
            observer.observe(&frame)?;
            if frame["type"] == "todo" {
                // The managed observer committed this snapshot to the durable outbox already.
                return Ok(false);
            }
        }
    }
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
    state.apply_ask_bridge_frame(subject, line);
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
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

// Wall-clock minute boundaries can be one second apart; confirmations need a full period.
const CHANNEL_TODO_END_CONFIRMATION_GAP: Duration = Duration::from_secs(60);

async fn remove_confirmed_ended_channel_todo_outbox(
    client: &Client, subject: &str, incarnation: &str, dir: &Path,
    end_seen: &mut Option<tokio::time::Instant>,
) -> Result<bool> {
    let status: Result<StatusResponse> = client.get(&format!(
        "/v1/status?subject={}", urlencoding::encode(subject),
    )).await;
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            *end_seen = None;
            return Err(error);
        }
    };
    let ended = status.subjects.first().and_then(|seat| seat.actual.as_ref())
        .is_some_and(|actual| todo_runtime_has_ended(actual, incarnation));
    if !ended {
        *end_seen = None;
        return Ok(false);
    }
    let first_seen = end_seen.get_or_insert_with(tokio::time::Instant::now);
    if first_seen.elapsed() >= CHANNEL_TODO_END_CONFIRMATION_GAP {
        fs::remove_dir_all(dir)?;
        return Ok(true);
    }
    Ok(false)
}

async fn finish_channel_todo_outbox(
    client: &Client, subject: &str, incarnation: &str, dir: &Path,
    end_seen: &mut Option<tokio::time::Instant>,
) -> Result<bool> {
    loop {
        if remove_confirmed_ended_channel_todo_outbox(
            client, subject, incarnation, dir, end_seen,
        ).await? {
            return Ok(true);
        }
        let Some(first_seen) = *end_seen else { return Ok(false); };
        // EOF is not an independent liveness confirmation until the same gap has elapsed.
        tokio::time::sleep_until(first_seen + CHANNEL_TODO_END_CONFIRMATION_GAP).await;
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
    // The latest validated hydration survives graph lag and binary replacement without rebinding.
    #[serde(default)]
    todo_pending: Option<Value>,
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
    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
    #[serde(default)]
    pending_ask_retry_sent: bool,
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
}

impl PiChannelResume {
    fn record_pending_todo(&mut self) -> Result<()> {
        if let Some(fields) = &self.todo_pending
            && let Some(dir) = &self.todo_outbox
        {
            st_drivers::harness_events::write_channel_todo(dir, &self.incarnation, fields)?;
            self.todo_pending = None;
        }
        Ok(())
    }

    // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
    fn take_pending_ask_retry(&mut self, frame: &Value, expected: Option<&str>) -> Option<String> {
        let id = frame["toolCallId"].as_str()?;
        if frame["type"] != "retry_pending_ask"
            || self.pending_ask_retry_sent
            || expected != Some(id)
        {
            return None;
        }
        self.pending_ask_retry_sent = true;
        Some(id.to_owned())
    }

    fn apply_ask_bridge_frame(&mut self, subject: &str, line: &str) {
        let Ok(frame) = serde_json::from_str::<Value>(line) else { return };
        let expected = std::env::var("ST3_OMP_PENDING_ASK").ok();
        if let Some(id) = self.take_pending_ask_retry(&frame, expected.as_deref()) {
            let sent = (|| -> Result<()> {
                let root = std::env::var_os("PTY_ROOT").context("pending ask retry has no PTY_ROOT")?;
                let session = std::env::var("PTY_SESSION").context("pending ask retry has no PTY_SESSION")?;
                st_runtime::PtyRuntime::new(root.into()).send_raw_if(&session, b"\x1b[15~", None)?;
                Ok(())
            })();
            let record = match sent {
                Ok(()) => json!({"type":"omp_pending_ask_retry","toolCallId":id}),
                Err(error) => json!({"type":"omp_pending_ask_retry_failed","toolCallId":id,"error":format!("{error:#}")}),
            };
            let _ = write_driver_log(subject, &record.to_string());
        }
        if frame["type"] == "diagnostic" {
            let _ = write_driver_log(subject, &frame.to_string());
        }
    }
    // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
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
                self.todo_pending = Some(json!(fields));
                if let Err(error) = self.record_pending_todo() {
                    tracing::warn!("st omp channel: recording todo failed: {error:#}");
                }
                true
            }
            // LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge — DELETE at contraction — https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf
            Some("delivery_ready") => {
                // Delivery readiness is independent of the reopened picker's human-blocked state.
                self.first_idle_seen = true;
                false
            }
            // LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge
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
                self.pending.background_jobs = frame.get("backgroundJobs").and_then(Value::as_u64);
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
                if self.native_session.as_deref() != Some(native) {
                    self.todo_pending = None;
                }
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
    background_jobs: Option<u64>,
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
                            ("background_jobs".into(), self.background_jobs.map(Value::from).unwrap_or(Value::Null)),
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
            match &self.fence {
                Some(fence) => mailbox_receipt(client, fence, &message, "read").await?,
                None => {
                    let _: ClaimRecord = client
                        .post(
                            &format!(
                                "/v1/messages/{}/claims",
                                urlencoding::encode(message.trim_start_matches("message/"))
                            ),
                            &MessageLifecycleRequest {
                                delegation: None,
                                lifecycle: "read".into(),
                                actor: Some(subject.into()),
                                transport: None,
                                runtime_id: None,
                                evidence: Vec::new(),
                                expected_subject: None,
                                idempotency_key: format!("pi-read:{subject}:{message}"),
                            },
                        )
                        .await?;
                }
            }
            // Both transports retain the native receipt until the graph acknowledges it.
            use tokio::io::AsyncWriteExt as _;
            let mut stdout = cli_output::async_stdout();
            stdout.write_all(format!("{}\n", json!({"type":"settled","meta":{"messageId":message}})).as_bytes()).await?;
            stdout.flush().await?;
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
                        "desired_token": std::env::var(st3::suspension::LAUNCH_DESIRED_TOKEN_ENV)
                            .ok().filter(|token| !token.is_empty()),
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

async fn message_content(client: &Client, message: &MessageView) -> Result<String> {
    if message.body_ref.is_some() {
        st3::message_body::full_text(client, message).await
    } else if message.content.starts_with("doc/") {
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

const DELIVERY_CONTROL_READ_DEADLINE: Duration = Duration::from_millis(250);
const DELIVERY_CONTROL_RETRY_DELAY: Duration = Duration::from_millis(100);
const DELIVERY_CONTROL_LEASE: Duration = Duration::from_secs(3);

#[cfg(test)]
async fn refresh_graph_delivery_gate(
    client: &Client,
    subject: &str,
    gate: &st_drivers::session_control::DeliveryGate,
) -> Result<()> {
    refresh_graph_delivery_gate_with_report(client, subject, gate, |_| {}).await
}

async fn refresh_graph_delivery_gate_with_report(
    client: &Client,
    subject: &str,
    gate: &st_drivers::session_control::DeliveryGate,
    mut report: impl FnMut(&st_drivers::session_control::DeliveryGate),
) -> Result<()> {
    let path = format!("/v1/delivery/hold?subject={}", urlencoding::encode(subject));
    for attempt in 0..2 {
        let result: Result<st3::delivery_hold::HoldView> = async {
            let view = tokio::time::timeout(
                DELIVERY_CONTROL_READ_DEADLINE,
                client.get::<st3::delivery_hold::HoldView>(&path),
            )
            .await
            .context("delivery hold read timed out")??;
            anyhow::ensure!(
                view.subject == subject,
                "delivery control names a different seat"
            );
            Ok(view)
        }
        .await;
        match result {
            Ok(view) => {
                gate.update(view.active, DELIVERY_CONTROL_LEASE);
                report(gate);
                return Ok(());
            }
            Err(error) => {
                // Close before retrying, even if the previous permit has time left. A late
                // response from the cancelled attempt can never authorize native input.
                gate.unavailable();
                if attempt == 1 {
                    // Publish only the final result so a recovered retry does not flap health.
                    report(gate);
                    return Err(error).context(
                        "new native handoffs held until graph delivery control is available",
                    );
                }
                // Exactly one retry: both 250 ms reads and this delay fit inside the lease.
                tokio::time::sleep(DELIVERY_CONTROL_RETRY_DELAY).await;
            }
        }
    }
    unreachable!("two attempts either read control or return the last error")
}

fn native_delivery_control_report(
    transport: &str,
    gate: &st_drivers::session_control::DeliveryGate,
) -> Value {
    use st_drivers::session_control::DeliveryBlockReason;
    let mut report: Value = serde_json::from_str(&native_delivery_report(transport, None))
        .expect("native delivery reports are JSON");
    // A deliberate graph hold is normal control, independent of mailbox readiness.
    let unavailable = gate.blocked_reason()
        .filter(|reason| *reason == DeliveryBlockReason::ControlUnavailable);
    report["ready"] = json!(unavailable.is_none());
    report["reason"] = json!(unavailable.map(|reason| reason.description()));
    report
}

async fn refresh_native_delivery_control(
    client: &Client,
    subject: &str,
    paths: &mut NativePaths,
    mut report: impl FnMut(&st_drivers::session_control::DeliveryGate),
) -> Result<()> {
    let adoption: Result<()> = async {
        if let Some(request) = &paths.pending_hold_adoption {
            // The previous hold can expire during an outage. Never import it with a new deadline.
            if current_unix_ms()? < u128::from(request.until_unix_ms) {
                tokio::time::timeout(
                    DELIVERY_CONTROL_READ_DEADLINE,
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
        paths.delivery_gate.unavailable();
        report(&paths.delivery_gate);
        return Err(error
            .context("native delivery held while the predecessor's hold awaits graph adoption"));
    }
    refresh_graph_delivery_gate_with_report(client, subject, &paths.delivery_gate, report).await
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
        .get(&format!("/v1/work?actor={}&caller=capacity-diagnostics", urlencoding::encode(subject)))
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
            "{}; the driver keeps running and retries with bounded backoff until the daemon is back",
            outage.unwrap_or_else(|| format!("the st daemon did not answer ({error:#})"))
        );
        let _ = write_driver_log(subject, &line);
        *last_warning = Some(now);
    }
    Ok(())
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

async fn renew_claimed_work(
    client: &Client,
    subject: &str,
    minute: u64,
    retry: &mut st3::client::DriverRetry,
) -> Result<()> {
    let work: Vec<StepRunView> = client
        .get(&format!("/v1/work?actor={}&view=renewal", urlencoding::encode(subject)))
        .await?;
    update_renewal_retry_lease(retry, &work, subject, current_unix_ms()?);
    // Most resident seats hold no work. Only a held claim needs the status reduction
    // that proves its harness incarnation is still live before renewing the lease.
    if !work.iter().any(|step| step.work_claim_is_held_by(subject)) {
        return Ok(());
    }
    let status: StatusResponse = client
        .get(&format!(
            "/v1/status?subject={}&harness_only=true",
            urlencoding::encode(subject)
        ))
        .await?;
    let harness = status
        .subjects
        .iter()
        .find(|candidate| candidate.subject == subject)
        .and_then(|candidate| candidate.harness.as_ref());
    let mut failure = None;
    for step in work
        .into_iter()
        .filter(|step| work_claim_has_active_harness(step, subject, harness))
    {
        let renewed: Result<StepRunView> = client
            .post(
                &format!("/v1/work/renew/{}", urlencoding::encode(&step.subject)),
                &WorkRequest {
                    wait_ms: None,
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

/// Called only after a complete, successful work read. A failed read or a held claim whose
/// expiry is unknown does not establish that a previous lease bound is no longer relevant.
fn update_renewal_retry_lease(
    retry: &mut st3::client::DriverRetry,
    work: &[StepRunView],
    subject: &str,
    now_ms: u128,
) {
    let mut held = work
        .iter()
        .filter(|step| step.work_claim_is_held_by(subject))
        .peekable();
    if held.peek().is_none() {
        retry.clear_lease_margin();
    } else if let Some(expiry) = held.filter_map(|step| step.claim_expires_at_unix_ms).min() {
        let remaining = expiry.saturating_sub(now_ms);
        retry.lease_margin(Duration::from_millis(remaining.min(u128::from(u64::MAX)) as u64));
    }
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
    step.work_claim_is_held_by(subject)
        && harness.is_some_and(|harness| {
            harness.state != "ended"
                && step.claim_incarnation.as_deref() == Some(harness.incarnation_id.as_str())
        })
}

/// Seconds into each minute at which this seat renews its work leases, 0 to 49, fixed per seat.
/// Every driver once renewed the moment the minute changed, so every running seat reached the
/// daemon's single writer in the same second and the renewals queued behind one another there
/// (about 45 of them, 2 to 70 ms each, an average wait of 800 ms and a tail of 2 s on the production host). A
/// lease lasts ten minutes and is renewed every minute, so an offset inside the minute costs it
/// nothing.
fn renewal_offset_secs(subject: &str) -> u64 {
    let digest = Sha256::digest(subject.as_bytes());
    u64::from(u16::from_be_bytes([digest[0], digest[1]])) % 50
}

/// The minute this seat should renew in once `unix_secs` is past its offset in that minute.
fn renewal_minute_at(subject: &str, unix_secs: u64) -> Option<u64> {
    (unix_secs % 60 >= renewal_offset_secs(subject)).then_some(unix_secs / 60)
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
        let backoff = if daemon_unreachable {
            Duration::from_secs(1)
        } else {
            st3::client::driver_retry_base(self.failures.saturating_sub(1))
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

fn admission_diagnostic(
    subject: &str,
    incarnation: &str,
    observed: &st_drivers::driver_diagnostic::Observed,
) -> Option<ClaimInput> {
    use st_drivers::driver_diagnostic::{Driver, Observed, Stage};
    let Observed::Failure(failure) = observed else {
        return None;
    };
    if failure.stage != Stage::VersionGate {
        return None;
    }
    let reason = format!(
        "{} {} admission failed at {}; native delivery is unavailable. {}",
        failure.driver.as_str(),
        failure
            .producer_version
            .as_deref()
            .unwrap_or("unknown version"),
        failure.reason.as_str(),
        st_drivers::driver_diagnostic::repair_text(observed)
    );
    Some(ClaimInput {
        subject: subject.into(),
        kind: "harness.diagnostic".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            (
                "severity".into(),
                json!(if failure.driver == Driver::Omp {
                    "error"
                } else {
                    "warning"
                }),
            ),
            (
                "status".into(),
                json!(if failure.driver == Driver::Omp {
                    "failed"
                } else {
                    "degraded"
                }),
            ),
            ("code".into(), json!("harness-admission-failed")),
            ("reason".into(), json!(reason)),
            ("incarnation_id".into(), json!(incarnation)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        idempotency_key: Some(format!(
            "harness-admission:{subject}:{incarnation}:{}:{}",
            failure.reason.as_str(),
            failure.observed_at
        )),
    })
}

async fn publish_admission_diagnostic(
    client: &Client,
    subject: &str,
    incarnation: &str,
    agent_dir: &Path,
    last: &mut Option<String>,
) -> Result<()> {
    let observed =
        st_drivers::driver_diagnostic::read(&st_drivers::driver_diagnostic::path(agent_dir));
    let Some(input) = admission_diagnostic(subject, incarnation, &observed) else {
        return Ok(());
    };
    if *last == input.idempotency_key {
        return Ok(());
    }
    let _: ClaimRecord = client.post("/v1/claims", &input).await?;
    *last = input.idempotency_key;
    Ok(())
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

/// Startup-only fallback for a channel whose provider never writes native state.
/// The single driver loop orders this before any subsequently published hook event.
/// A native state permanently suppresses it, including failed/unknown/terminal state.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct ClaudeReadinessFallback {
    initialized_at_ms: Option<u64>,
    native_state_seen: bool,
}
impl ClaudeReadinessFallback {
    fn observe_attachment(&mut self, checked: &Result<st3::mailbox::Attachment>, now_ms: u64) {
        if checked.as_ref().is_ok_and(|attachment| attachment.attached) {
            self.initialized_at_ms.get_or_insert(now_ms);
        } else {
            self.initialized_at_ms = None;
        }
    }
}

async fn publish_claude_readiness_fallback(
    client: &Client,
    mailbox: &NativeMailbox,
    observations: &mut NativeObservations,
    state: &mut NativeLoopState,
    now_ms: u64,
) -> Result<bool> {
    state.claude_readiness_fallback.native_state_seen |= observations.native_state_seen;
    if !observations.enabled
        || state.ready
        || state.claude_readiness_fallback.native_state_seen
        || !state
            .claude_readiness_fallback
            .initialized_at_ms
            .is_some_and(|started| now_ms.saturating_sub(started) >= 15_000)
    {
        return Ok(false);
    }
    let fence = &mailbox.fence;
    anyhow::ensure!(
        fence.incarnation == observations.runtime && fence.component == "title",
        "the Claude readiness fallback belongs to another native mailbox incarnation"
    );
    // A bounded drain may leave a backlog. The current runtime snapshot still reveals
    // native state already queued behind that backlog, without granting readiness.
    if let Some(raw) =
        st_drivers::harness_events::read_runtime_state(&observations.dir, &observations.runtime)?
    {
        let observed = st_drivers::harness_state::read_raw_at(&raw, None, now_ms);
        if !(observed.state == st_drivers::harness_state::Activity::Unknown
            && observed.reason.as_deref() == Some("claimed"))
        {
            observations.native_state_seen = true;
            state.claude_readiness_fallback.native_state_seen = true;
            return Ok(false);
        }
    }
    let checked =
        checked_claude_attachment(client, &fence.subject, &fence.incarnation, fence).await;
    state
        .claude_readiness_fallback
        .observe_attachment(&checked, now_ms);
    if !checked?.attached {
        return Ok(false);
    }
    // Consume queued native state first, independently of the wake/tick select order.
    // If publication fails, no readiness claim may bypass that native observation.
    let drained = observations
        .drain_live(client, &fence.subject, "claude", &mut state.ready)
        .await;
    state.claude_readiness_fallback.native_state_seen |= observations.native_state_seen;
    if drained? {
        return Ok(true);
    }
    if state.ready || state.claude_readiness_fallback.native_state_seen {
        return Ok(false);
    }
    let _: ClaimRecord = client
        .post(
            "/v1/claims",
            &ClaimInput {
                subject: fence.subject.clone(),
                kind: "harness.observed".into(),
                actor: Some(fence.subject.clone()),
                fields: with_quiescence(BTreeMap::from([
                    ("state".into(), json!("ready")),
                    ("driver".into(), json!("claude")),
                    ("transport".into(), json!("claude-channel")),
                    ("incarnation_id".into(), json!(fence.incarnation)),
                    (
                        "reason".into(),
                        json!("channelInitializedWithoutNativeState"),
                    ),
                ])),
                evidence: vec![],
                expected_subject: None,
                idempotency_key: Some(format!(
                    "claude-hookless-ready:{}:{}",
                    fence.subject, fence.incarnation
                )),
            },
        )
        .await?;
    state.ready = true;
    Ok(false)
}

async fn check_claude_attachment(
    client: &Client,
    subject: &str,
    incarnation: &str,
    mailbox: &NativeMailbox,
    started: Instant,
    state: &mut NativeLoopState,
) -> Result<()> {
    let fence = &mailbox.fence;
    let mut checked = checked_claude_attachment(client, subject, incarnation, fence).await;
    state.claude_readiness_fallback.observe_attachment(&checked, current_unix_ms()? as u64);
    // Readiness is independent of diagnostic publication. A rejected or uncertain
    // POST must not silence a current subscription's readiness report.
    report_claude_attachment(mailbox, &checked, state.claude_attachment_terminal.as_ref())?;
    if let Some(terminal) = &state.claude_attachment_terminal {
        if same_claude_attachment_binding(&terminal.fence, fence) {
            return Err(claude_attachment_parked_error(terminal));
        }
        // A changed binding is not sufficient: the endpoint must admit its current
        // ownership before this publisher leaves its parked state.
        checked.as_ref().map_err(|error| {
            anyhow::anyhow!("validating replacement Claude attachment ownership: {error:#}")
        })?;
        state.claude_attachment_terminal = None;
        state.claude_attachment_reconciled = false;
    }
    if state.claude_attachment_pending.is_some() {
        // Resolve the identical operation before advancing the acknowledged episode.
        let published =
            publish_pending_claude_attachment(client, subject, incarnation, fence, state).await;
        finish_claude_attachment_publication(published, mailbox, &checked, state)?;
        // Recovery publication needs an admission check AFTER the pending operation
        // resolved; the earlier check was only used to keep readiness reporting.
        checked = checked_claude_attachment(client, subject, incarnation, fence).await;
        report_claude_attachment(mailbox, &checked, state.claude_attachment_terminal.as_ref())?;
    }
    let attached = checked.as_ref().is_ok_and(|attachment| attachment.attached);
    let phase = if attached {
        "attached"
    } else if started.elapsed() >= claude_attachment_window()
        || state.claude_attachment_phase == "blocked"
    {
        "blocked"
    } else {
        "starting"
    };
    if state.claude_attachment_reconciled && state.claude_attachment_phase == phase {
        return Ok(());
    }
    let reason = claude_attachment_reason(&checked);
    let input =
        claude_attachment_diagnostic(fence, state.claude_attachment_episode, phase, &reason);
    let input_digest = claude_attachment_input_digest(&input)?;
    // Retained in memory before POST; only the existing graceful re-exec write_state
    // serializes it to disk. This does not promise recovery from abrupt process death.
    state.claude_attachment_pending = Some(PendingClaudeAttachment {
        fence: fence.clone(),
        phase: phase.into(),
        input,
        input_digest,
    });
    state.claude_attachment_reconciled = false;
    let published =
        publish_pending_claude_attachment(client, subject, incarnation, fence, state).await;
    finish_claude_attachment_publication(published, mailbox, &checked, state)
}

fn finish_claude_attachment_publication(
    published: Result<()>,
    mailbox: &NativeMailbox,
    checked: &Result<st3::mailbox::Attachment>,
    state: &NativeLoopState,
) -> Result<()> {
    // Terminal retirement must be visible in THIS tick, using the already admitted
    // readiness result. ACK also clears any old parked metadata immediately.
    if published.is_ok() || state.claude_attachment_terminal.is_some() {
        let reported =
            report_claude_attachment(mailbox, checked, state.claude_attachment_terminal.as_ref());
        if let Err(report_error) = reported {
            return match published {
                Err(error) => Err(error.context(format!(
                    "reporting attachment publication outcome: {report_error:#}"
                ))),
                Ok(()) => Err(report_error),
            };
        }
    }
    published
}

/// How long a Claude seat may start before an unattached channel is reported as blocked.
/// `ST3_CLAUDE_ATTACH_WINDOW_MS` can only shorten it, as `ST3_SUBAGENT_LEASE_MS` does a lease, so
/// the boot canaries need not wait out the production window.
fn claude_attachment_window() -> Duration {
    const DEFAULT_MS: u64 = 20_000;
    let cap = std::env::var("ST3_CLAUDE_ATTACH_WINDOW_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|cap| *cap > 0);
    Duration::from_millis(cap.map_or(DEFAULT_MS, |cap| cap.min(DEFAULT_MS)))
}

fn same_claude_attachment_binding(a: &st3::mailbox::Fence, b: &st3::mailbox::Fence) -> bool {
    a.subject == b.subject
        && a.incarnation == b.incarnation
        && a.component == b.component
        && a.epoch == b.epoch
        && a.token == b.token
}

async fn checked_claude_attachment(
    client: &Client,
    subject: &str,
    incarnation: &str,
    fence: &st3::mailbox::Fence,
) -> Result<st3::mailbox::Attachment> {
    anyhow::ensure!(
        fence.subject == subject && fence.incarnation == incarnation,
        "the Claude attachment check belongs to another mailbox binding"
    );
    tokio::time::timeout(
        Duration::from_secs(2),
        client.get(&claude_attachment_path(fence)),
    )
    .await
    .context("the channel attachment check exceeded two seconds")?
}

fn claude_attachment_reason(checked: &Result<st3::mailbox::Attachment>) -> String {
    match checked {
        Ok(attachment) if attachment.attached => "claude-channel-attached: the current Claude channel is initialized and subscribed.".into(),
        Ok(_) => "claude-channel-unattached: the current Claude session has no live, initialized channel subscription; mail is held in the graph until attachment. The driver rechecks attachment and st will restart the harness with bounded retries if the channel stays missing.".into(),
        Err(error) => format!("claude-channel-unattached: attachment could not be verified; mail is held while the driver retries: {error:#}"),
    }
}

fn report_claude_attachment(
    mailbox: &NativeMailbox,
    checked: &Result<st3::mailbox::Attachment>,
    terminal: Option<&ClaudeAttachmentTerminal>,
) -> Result<()> {
    // A predecessor binding's parked state does not describe this new binding.
    let terminal =
        terminal.filter(|fault| same_claude_attachment_binding(&fault.fence, &mailbox.fence));
    let mut report: Value = serde_json::from_str(&native_delivery_report("claude-channel", None))?;
    report["ready"] = json!(checked.as_ref().is_ok_and(|attachment| attachment.attached));
    report["reason"] = json!(match terminal {
        Some(fault) => format!(
            "{}; attachment diagnostic publication is parked: {}. The durable graph fence has not been corrected; recovery requires an authorized replacement binding.",
            claude_attachment_reason(checked),
            fault.reason
        ),
        None => claude_attachment_reason(checked),
    });
    report["attachment_diagnostic_publication"] = match terminal {
        Some(fault) => {
            json!({"state":"parked", "reason":fault.reason, "incarnation":fault.fence.incarnation, "epoch":fault.fence.epoch})
        }
        None => Value::Null,
    };
    if let Some(subscription) = &mailbox.subscription {
        subscription.report(report);
    }
    Ok(())
}

fn claude_attachment_diagnostic(
    fence: &st3::mailbox::Fence,
    episode: u64,
    phase: &str,
    reason: &str,
) -> ClaimInput {
    let subject = &fence.subject;
    let incarnation = &fence.incarnation;
    let attached = phase == "attached";
    let code = if attached {
        "claude-channel-attached"
    } else {
        "claude-channel-unattached"
    };
    ClaimInput {
        subject: subject.into(),
        kind: "harness.diagnostic".into(),
        actor: Some(subject.into()),
        fields: BTreeMap::from([
            (
                "severity".into(),
                json!(if phase == "blocked" {
                    "error"
                } else {
                    "warning"
                }),
            ),
            (
                "status".into(),
                json!(if attached { "recovered" } else { phase }),
            ),
            ("code".into(), json!(code)),
            (
                "reason".into(),
                json!(if attached {
                    "The current Claude channel is initialized and subscribed; durable mail delivery resumes."
                } else {
                    reason
                }),
            ),
            ("driver".into(), json!("claude")),
            ("incarnation_id".into(), json!(incarnation)),
        ]),
        evidence: Vec::new(),
        expected_subject: None,
        // A new namespace avoids reusing a predecessor image's uncertain key with
        // a reason that may have changed since that image attempted publication.
        // Abrupt process death loses the pending slot and episode. If a caller then
        // reuses this epoch/episode/phase with a different reason, the same-key
        // mismatch parks publication; this safeguard covers graceful re-exec only.
        idempotency_key: Some(format!(
            "claude-attachment-publication-v2:{code}:{subject}:{incarnation}:{}:{episode}:{phase}",
            fence.epoch
        )),
    }
}

fn claude_attachment_input_digest(input: &ClaimInput) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(input)?)))
}

async fn publish_pending_claude_attachment(
    client: &Client,
    subject: &str,
    incarnation: &str,
    fence: &st3::mailbox::Fence,
    state: &mut NativeLoopState,
) -> Result<()> {
    let Some(pending) = state.claude_attachment_pending.as_ref() else {
        return Ok(());
    };
    let valid_binding = fence.subject == subject
        && fence.incarnation == incarnation
        && same_claude_attachment_binding(&pending.fence, fence)
        && pending.input.subject == subject
        && pending.input.actor.as_deref() == Some(subject)
        && pending
            .input
            .fields
            .get("incarnation_id")
            .and_then(Value::as_str)
            == Some(incarnation);
    if !valid_binding {
        return Err(retire_claude_attachment(
            state,
            fence,
            "the pending Claude attachment diagnostic belongs to another mailbox binding",
        ));
    }
    let valid_input = (|| -> Result<bool> {
        let reason = pending
            .input
            .fields
            .get("reason")
            .and_then(Value::as_str)
            .context("the pending Claude attachment diagnostic has no reason")?;
        Ok(
            matches!(pending.phase.as_str(), "starting" | "blocked" | "attached")
                && pending.input_digest == claude_attachment_input_digest(&pending.input)?
                && serde_json::to_value(&pending.input)?
                    == serde_json::to_value(claude_attachment_diagnostic(
                        fence,
                        state.claude_attachment_episode,
                        &pending.phase,
                        reason,
                    ))?,
        )
    })();
    if !matches!(valid_input, Ok(true)) {
        return Err(retire_claude_attachment(
            state,
            fence,
            "the pending Claude attachment diagnostic request was changed or malformed",
        ));
    }
    let Some(next_episode) = state.claude_attachment_episode.checked_add(1) else {
        return Err(retire_claude_attachment(
            state,
            fence,
            "the Claude attachment diagnostic episode overflowed",
        ));
    };
    // Validate the current title binding before retrying a retained operation.
    // This result is not reused as recovery proof.
    let result: Result<ClaimRecord> = async {
        let _ = checked_claude_attachment(client, subject, incarnation, fence).await?;
        client.post("/v1/claims", &pending.input).await
    }
    .await;
    if let Err(error) = result {
        // A mismatch of this SAME immutable request/key or a checkpointed claim cannot
        // be resolved by reposting it; park visibly without assuming whether it committed.
        // Authentication renewal, generic conflicts and rate limits do not establish
        // a permanent outcome. Keep their uncertain operation unchanged.
        if matches!(
            st3::client::api_error_code(&error),
            Some(
                "idempotency-mismatch"
                    | "claim-checkpointed"
                    | "stale-mailbox-session"
                    | "foreign-mailbox"
                    | "invalid-mailbox-token"
                    | "unknown-claim-kind"
                    | "invalid-claim-actor"
                    | "unknown-claim-field"
            )
        ) {
            return Err(retire_claude_attachment(
                state,
                fence,
                &format!("{error:#}"),
            ));
        }
        return Err(error);
    }
    state.claude_attachment_phase = pending.phase.clone();
    state.claude_attachment_episode = next_episode;
    state.claude_attachment_reconciled = true;
    state.claude_attachment_pending = None;
    Ok(())
}

fn retire_claude_attachment(
    state: &mut NativeLoopState,
    fence: &st3::mailbox::Fence,
    reason: &str,
) -> anyhow::Error {
    let reason: String = reason.chars().take(2_000).collect();
    state.claude_attachment_pending = None;
    state.claude_attachment_terminal = Some(ClaudeAttachmentTerminal {
        fence: fence.clone(),
        reason: reason.clone(),
    });
    // The fault remains visible on every tick under the driver's existing warning
    // throttle, but network publication is capped at the one rejected operation.
    // Neither phase nor acknowledged episode advances; graceful re-exec retains it.
    anyhow::anyhow!(
        "Claude attachment diagnostic publication stopped for this binding: {reason}; no further POSTs; recovery requires an authorized replacement binding"
    )
}

fn claude_attachment_parked_error(fault: &ClaudeAttachmentTerminal) -> anyhow::Error {
    anyhow::anyhow!(
        "Claude attachment diagnostic publication is parked for incarnation {} epoch {}: {}; no further POSTs; the durable graph fence is unchanged; recovery requires an authorized replacement binding",
        fault.fence.incarnation,
        fault.fence.epoch,
        fault.reason
    )
}

fn claude_attachment_path(fence: &st3::mailbox::Fence) -> String {
    format!(
        "/v1/mailbox/attachment?subject={}&incarnation={}&component={}&epoch={}&token={}",
        urlencoding::encode(&fence.subject),
        urlencoding::encode(&fence.incarnation),
        fence.component,
        fence.epoch,
        fence.token,
    )
}

/// The daemon's refusal of a binding whose provider has already ended.
fn provider_ended_before_bind(error: &anyhow::Error) -> bool {
    st3::client::api_error_code(error) == Some("stale-mailbox-session")
        && provider_ended_refusal(&format!("{error:#}"))
}

/// The text of that refusal, as a bind error or a fenced subscription carries it.
fn provider_ended_refusal(text: &str) -> bool {
    text.contains("stale-mailbox-session") && text.contains("the provider authority is terminal")
}

/// Whether the provider's task has returned, waiting briefly for it: its terminal record is
/// written just before it returns, so the refusal can arrive a moment ahead of the task.
async fn provider_task_settled(task: &tokio::task::JoinHandle<Result<()>>) -> bool {
    let settled = async {
        while !task.is_finished() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), settled).await.is_ok()
}

struct NativeMailbox {
    subscription: Option<st3::mailbox::Subscription>,
    fence: st3::mailbox::Fence,
    messages: Vec<MessageView>,
    queued: BTreeMap<String, st_drivers::message::Message>,
    replayed: bool,
    last_title_warning: Option<Instant>,
}
impl NativeMailbox {
    /// A mailbox with no subscription, for a provider that ended before it could be bound.
    fn unbound(subject: &str, incarnation: &str, driver: &str, state: &mut NativeLoopState) -> Self {
        let component = if matches!(driver, "codex" | "opencode") {
            "delivery"
        } else {
            "title"
        };
        let fence = state
            .mailbox_fence
            .get_or_insert_with(|| st3::mailbox::Fence::new(subject, incarnation, component))
            .clone();
        Self {
            subscription: None,
            fence,
            messages: Vec::new(),
            queued: BTreeMap::new(),
            replayed: false,
            last_title_warning: None,
        }
    }
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
        let report = if matches!(driver, "codex" | "opencode") {
            native_delivery_control_report(transport, &st_drivers::session_control::DeliveryGate::default())
        } else {
            serde_json::from_str(&native_delivery_report(transport, None)).expect("native report is JSON")
        };
        let subscription = push_mailbox_enabled()
            .then(|| st3::mailbox::Subscription::start(client.clone(), fence.clone(), report));
        Ok(Self {
            subscription,
            fence,
            messages: Vec::new(),
            queued: BTreeMap::new(),
            replayed: false,
            last_title_warning: None,
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
            Some(st3::mailbox::Frame::Replay { nonce }) => {
                if let Some(subscription) = &self.subscription { subscription.acknowledge_replay(nonce); }
                Ok(())
            }
            Some(st3::mailbox::Frame::Drain { operation }) => {
                if let Some(subscription) = &self.subscription {
                    subscription.acknowledge_drain(operation);
                }
                Ok(())
            }
            Some(st3::mailbox::Frame::Seat { seat }) => {
                #[cfg(feature = "test-support")]
                if env!("CARGO_BIN_NAME") == "st3-fixture"
                    && let Some(root) = std::env::var_os("ST3_FIXTURE_TERMINAL_COMPLETION")
                {
                    fs::write(PathBuf::from(root).join("title-stream-admitted"), b"seat")?;
                }
                if let Err(error) = update_native_title(&seat, runtime_id) {
                    let now = Instant::now();
                    if self.last_title_warning.is_none_or(|prior| {
                        now.duration_since(prior) >= Duration::from_secs(10)
                    }) {
                        let _ = write_driver_log(
                            &self.fence.subject,
                            &format!("could not update seat title: {error:#}"),
                        );
                        self.last_title_warning = Some(now);
                    }
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
        queued.sort_by(|left, right| {
            (left.ts_ms, &left.filename).cmp(&(right.ts_ms, &right.filename))
        });
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
        view.tags
            .iter()
            .filter(|tag| {
                (tag.as_str() == st3::silent::SILENT_TAG && st3::silent::is_held(view))
                    || tag.as_str() == "dictated"
                    || tag.starts_with(st3::silent::REMAINING_PREFIX)
            })
            .cloned(),
    );
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
    // The launcher's registry name can differ from the provider's logical runtime ID.
    let session = std::env::var("PTY_SESSION")
        .ok()
        .filter(|session| !session.is_empty())
        .unwrap_or_else(|| runtime_id.replace('/', "."));
    let result = std::process::Command::new("pty")
        .args(["rename", &session, &label])
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
    let mut pending_silent = Vec::new();
    loop {
        let page = message_page_reporting(
            client,
            Some(subject),
            false,
            cursor.as_deref(),
            if cursor.is_none() { report } else { None },
        )
        .await?;
        let mut items = page.items;
        // Held mail goes to the seat only beside a message that wakes it.
        st3::silent::release_page(&mut items, &mut pending_silent);
        for message in items {
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
                    let content = if message.body_ref.is_some() {
                        st3::message_body::full_text(client, &message).await?
                    } else if message.content.starts_with("doc/") {
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
async fn enforce_account_limits(store: Arc<Store>, started_with: st3::config::LimitsConfig, reconciler: Arc<Reconciler>) {
    const LIMITS_INTERVAL: Duration = Duration::from_secs(2 * 60);
    let mut limits = started_with;
    let mut last_error = None::<String>;
    loop {
        // A config file that is missing, cannot be read or does not validate keeps the last
        // good policy. The reason is logged when it changes, not on every pass.
        let reloaded = tokio::task::spawn_blocking(st3::config::reload_daemon_policies)
            .await
            .ok()
            .flatten();
        match reloaded {
            Some(Ok((reloaded, reconcile))) => {
                // Validation already passed; preserve last-start history while changing the rate.
                reconciler.set_max_passes_per_minute(reconcile.max_passes_per_minute)
                    .expect("validated reconcile cap");
                reconciler.set_idle_nudge(reconcile.idle_nudge().expect("validated idle nudge"));
                limits = reloaded;
                last_error = None;
            }
            Some(Err(error)) => {
                let error = format!("{error:#}");
                if last_error.as_ref() != Some(&error) {
                    eprintln!("st3: daemon policies keep their last config: {error}");
                    last_error = Some(error);
                }
            }
            None => {}
        }
        let Some(policy) = st3::store::LimitsPolicy::from_config(&limits) else {
            tokio::time::sleep(LIMITS_INTERVAL).await;
            continue;
        };
        let pass = store.clone();
        match pass.connection.write_workers().spawn_blocking(move || {
            st3::profile::task("task enforce-account-limits", || {
                let now = now_ms();
                let check = pass.account_limits_check(now, policy.fresh_ms)?;
                let outcome = pass.enforce_account_limits(&policy, now)?;
                Ok::<_, anyhow::Error>((outcome, check))
            })
        })
        .await
        {
            Ok(Ok((outcome, check))) => {
                if check.status == "warn" {
                    eprintln!("st3: limits policy: {}", check.message);
                }
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

// One worker coalesces numeric contention generations. Three scheduled calls per episode,
// at 30/60/120 seconds, preserve the existing 30 s catch-up throttle. A later external
// contention can start a new episode; persistent failure alone cannot reset this budget.
#[derive(Default)]
struct ProjectionContentionRetry {
    observed: u64,
    attempt: usize,
    due: Option<tokio::time::Instant>,
}
impl ProjectionContentionRetry {
    const DELAYS: [Duration; 3] = [
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(120),
    ];
    fn poll(
        &mut self,
        now: tokio::time::Instant,
        generation: u64,
        deferred: bool,
    ) -> Option<usize> {
        if !deferred {
            self.observed = generation;
            self.attempt = 0;
            self.due = None;
            return None;
        }
        if self.due.is_none() && generation != self.observed {
            self.observed = generation;
            self.attempt = 0;
            self.due = Some(now + Self::DELAYS[0]);
        }
        if self.due.is_none_or(|due| now < due) {
            return None;
        }
        self.due = None;
        self.attempt += 1;
        Some(self.attempt)
    }
    fn finish(&mut self, now: tokio::time::Instant, generation: u64, contention_or_throttle: bool) {
        self.observed = generation;
        self.due = if contention_or_throttle && self.attempt < Self::DELAYS.len() {
            Some(now + Self::DELAYS[self.attempt])
        } else {
            None
        };
    }
}

fn retry_projection_contention(
    store: std::sync::Weak<Store>,
    notify: Arc<Notify>,
    event_notify: watch::Sender<u64>,
    state_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    retry_projection_contention_observed(store, notify, event_notify, state_dir, |_| {})
}

// The observer is a no-op in the daemon; owned tests use it to synchronize on actual
// backoff entry rather than assume a task has polled after a virtual-time advance.
fn retry_projection_contention_observed(
    store: std::sync::Weak<Store>,
    notify: Arc<Notify>,
    event_notify: watch::Sender<u64>,
    state_dir: PathBuf,
    mut on_poll: impl FnMut(bool) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut schedule = ProjectionContentionRetry::default();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(store) = store.upgrade() else { break };
            let generation = store.projection_contention_generation();
            let attempt = schedule.poll(
                tokio::time::Instant::now(), generation, store.replication_projection_deferred());
            on_poll(schedule.due.is_some());
            let Some(attempt) = attempt else { continue };
            // Only this task schedules these attempts. The existing projection mutex also
            // serializes them with receive/wake passes. Store ownership ends at attempt return.
            let result = store.connection.write_workers().spawn_blocking(move || {
                st3::profile::task("task replication-projection-retry", || {
                    let result = store.project_replication_backlog_unless_catching_up();
                    (result, store.projection_contention_generation(), store.replication_projection_deferred())
                })
            })
            .await;
            let still_deferred = match &result {
                Ok((_, _, deferred)) => Some(*deferred),
                Err(_) => None,
            };
            let (recovered, retry, generation, outcome) = match result {
                // Own success cannot advance contention generation. Retain the pre-call
                // watermark, so an external contention racing the success is observed on
                // the next poll rather than cancelled as part of the completed episode.
                Ok((Ok(Some(true)), _current, _)) => (true, false, generation, "projected"),
                Ok((Ok(None), current, _)) => (false, true, current, "catch-up-throttled"),
                Ok((Ok(Some(false)), current, _)) => {
                    (false, current != generation, current, "deferred")
                }
                Ok((Err(error), current, _)) => {
                    let busy = error
                        .downcast_ref::<smallclaims::Error>()
                        .is_some_and(|error| error.is_sqlite_contention());
                    (
                        false,
                        busy,
                        current,
                        if busy {
                            "sqlite-contention"
                        } else {
                            "other-error"
                        },
                    )
                }
                Err(_) => (false, false, generation, "worker-failed"),
            };
            schedule.finish(tokio::time::Instant::now(), generation, retry);
            eprintln!(
                "st3: projection contention retry {}",
                json!({
                    "attempt":attempt, "limit":3, "outcome":outcome, "rescheduled":schedule.due.is_some(),
                    "projection_deferred_after_attempt": still_deferred,
                    "stop_reason": if schedule.due.is_some() { None } else if recovered {
                        Some("recovered")
                    } else if retry { Some("budget-exhausted") } else { Some("non-retryable") },
                })
            );
            if recovered {
                st3::performance::record_wake("projection-contention-retry", None);
                notify.notify_one();
                event_notify.send_modify(|generation| *generation = generation.saturating_add(1));
                let _ = fs::write(
                    state_dir.join("replication.wake"),
                    format!("{}\n", uuid::Uuid::now_v7()),
                );
            }
        }
    })
}

#[cfg(test)]
mod projection_contention_retry_tests;

// Ordinary samples and abnormal attempts have independent, bounded buckets. An ordinary
// sample cannot hide the first slow/error attempt. Repeated anomalies retain counts and
// maximum phase durations until the next allowed anomaly; shutdown does not flush them.
#[derive(Default)]
struct WalCheckpointLogBucket {
    last: Option<Instant>,
    suppressed: u64,
    errors: u64,
    max_passive_ms: u128,
    max_writer_wait_ms: u128,
    max_truncate_ms: u128,
    max_duration_ms: u128,
}
impl WalCheckpointLogBucket {
    fn record(
        &mut self,
        now: Instant,
        report: Option<&smallclaims::sqlite::WalCheckpointReport>,
        duration_ms: u128,
    ) -> Option<serde_json::Value> {
        self.max_duration_ms = self.max_duration_ms.max(duration_ms);
        if let Some(report) = report {
            self.max_passive_ms = self.max_passive_ms.max(report.passive_ms);
            self.max_writer_wait_ms = self.max_writer_wait_ms.max(report.writer_wait_ms);
            self.max_truncate_ms = self.max_truncate_ms.max(report.truncate_ms.unwrap_or(0));
        } else {
            self.errors = self.errors.saturating_add(1);
        }
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(60))
        {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        let retained = json!({
            "suppressed_outcomes": self.suppressed,
            "error_outcomes": self.errors,
            "max_passive_ms": self.max_passive_ms,
            "max_writer_wait_ms": self.max_writer_wait_ms,
            "max_truncate_ms": self.max_truncate_ms,
            "max_duration_ms": self.max_duration_ms,
        });
        *self = Self {
            last: Some(now),
            ..Self::default()
        };
        Some(retained)
    }
}
/// Counts checkpoint samples in a row that left part of the WAL un-copied and copied no more
/// than the sample before. This is neither holder identity nor snapshot age; errors retain
/// the previous count without advancing it.
#[derive(Default)]
struct WalPinTracker {
    last_backfilled: Option<i32>,
    stuck: u32,
}

impl WalPinTracker {
    /// Samples in a row, including this one, that made no backfill progress.
    fn observe(&mut self, report: Option<&smallclaims::sqlite::WalCheckpointReport>) -> u32 {
        let Some(report) = report else { return self.stuck };
        let pinned = report.frames > report.backfilled
            && self.last_backfilled == Some(report.backfilled);
        self.last_backfilled = Some(report.backfilled);
        self.stuck = if pinned { self.stuck + 1 } else { 0 };
        self.stuck
    }
}

const WAL_PIN_REPORT_AFTER: u32 = 3;

fn abnormal_wal_checkpoint(report: Option<&smallclaims::sqlite::WalCheckpointReport>) -> bool {
    report.is_none_or(|report| {
        report.passive_ms >= 1000
            || report.writer_wait_ms >= 100
            || report.truncate_ms.is_some_and(|ms| ms >= 100)
    })
}

#[cfg(test)]
mod wal_pin_tracker_tests;

#[cfg(test)]
mod wal_checkpoint_reporting_tests;

/// Page copying runs off the writer queue; TRUNCATE joins it before taking SQLite's writer
/// lock. Recycling has no SQLite busy wait for readers; it can wait its FIFO turn.
fn recycle_idle_wal(path: PathBuf, store: std::sync::Weak<Store>) {
    // Five seconds finds gaps between short readers without polling on every write.
    const WAL_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);
    const WAL_CHECKPOINT_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);

    // Backfill can take tens of seconds even with a zero busy timeout. A detached native
    // thread owns its connection so Tokio shutdown never waits for this best-effort work
    // in its blocking pool. Dropping the handle also avoids joining it at process exit.
    let worker = std::thread::Builder::new()
        .name("st3-wal-checkpoint".into())
        .spawn(move || {
            let mut connection = None;
            let mut last_error_log: Option<Instant> = None;
            let mut retry_interval = WAL_CHECKPOINT_INTERVAL;
            let mut ordinary_outcomes = WalCheckpointLogBucket::default();
            let mut abnormal_outcomes = WalCheckpointLogBucket::default();
            let mut pin = WalPinTracker::default();
            loop {
                std::thread::sleep(retry_interval);
                retry_interval = WAL_CHECKPOINT_INTERVAL;
                let started = Instant::now();
                let trace_started = std::time::SystemTime::now();
                // A weak reference does not keep the Store alive between attempts at shutdown.
                let Some(store) = store.upgrade() else { break };
                // The connection is taken into the attempt and dropped on unwind; no
                // potentially panic-damaged connection is reused by the next attempt.
                let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let connection = match connection.take() {
                        Some(connection) => connection,
                        None => rusqlite::Connection::open(&path)
                            .context("open WAL checkpoint connection")?,
                    };
                    let result = store.checkpoint_idle_wal_report(&connection);
                    Ok::<_, anyhow::Error>((connection, result))
                }));
                let result = match attempt {
                    Ok(Ok((returned, result))) => {
                        connection = Some(returned);
                        result.context("recycle idle WAL")
                    }
                    Ok(Err(error)) => Err(error),
                    Err(_) => {
                        // catch_unwind still invokes the process panic hook. Back off
                        // its retries too, without changing the hook for other threads.
                        retry_interval = WAL_CHECKPOINT_ERROR_LOG_INTERVAL;
                        Err(anyhow::anyhow!("WAL checkpoint panicked; reopening connection"))
                    }
                };
                let now = Instant::now();
                let duration_ms = started.elapsed().as_millis();
                let report = result.as_ref().ok();
                let stuck = pin.observe(report);
                let abnormal = abnormal_wal_checkpoint(report) || stuck >= WAL_PIN_REPORT_AFTER;
                st3::otel::record_wal_checkpoint(trace_started, report, abnormal);
                let bucket = if abnormal { &mut abnormal_outcomes } else { &mut ordinary_outcomes };
                if let Some(retained) = bucket.record(now, report, duration_ms) {
                    let outcome = match report {
                        Some(report) => json!({
                            "outcome": if report.frames == 0 { "empty" } else if report.recycled { "recycled" } else { "deferred" },
                            "frames": report.frames, "backfilled": report.backfilled,
                            "passive_ms": report.passive_ms, "writer_wait_ms": report.writer_wait_ms,
                            "truncate_ms": report.truncate_ms,
                        }),
                        None => json!({"outcome":"error", "phase_durations_available":false}),
                    };
                    let pinned = (stuck >= WAL_PIN_REPORT_AFTER).then(|| {
                        json!({
                            "stuck_samples": stuck,
                            "read_lifetimes": store.readers.live_read_report(),
                        })
                    });
                    eprintln!("st3: WAL checkpoint {}", json!({
                        "bucket": if abnormal { "abnormal" } else { "ordinary" },
                        "observed_at_unix_ms": std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH).ok().map(|age| age.as_millis()),
                        "report": outcome, "duration_ms": duration_ms, "retained": retained,
                        "pinned": pinned,
                    }));
                }
                if let Err(error) = result {
                    let now = Instant::now();
                    if last_error_log.is_none_or(|last| {
                        now.duration_since(last) >= WAL_CHECKPOINT_ERROR_LOG_INTERVAL
                    }) {
                        eprintln!("st3: {error:#}; checkpoint duration_ms={}", started.elapsed().as_millis());
                        last_error_log = Some(now);
                    }
                }
            }
        });
    if let Err(error) = worker {
        eprintln!("st3: start WAL checkpoint worker: {error}");
    }
}

/// Old payloads convert after startup; each page joins the normal writer queue and commits
/// its own cursor. A failed page retries, including after a daemon restart.
async fn convert_envelope_payloads(store: Arc<Store>) {
    loop {
        let store = store.clone();
        let result = store.connection.write_workers().spawn_blocking(move || store.convert_envelope_payloads()).await;
        match result {
            Ok(Ok(report)) if report.done => return,
            Ok(Ok(_)) => tokio::time::sleep(Duration::from_millis(50)).await,
            error => {
                eprintln!("st3: binary envelope conversion failed: {error:?}");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    }
}

/// Exactly one awaited bootstrap job, then a pause longer than the writer's batch window.
/// This never wakes a seat and stops once historical count metadata is complete.
async fn catch_up_coordination_counts(store: Arc<Store>) {
    loop {
        let page_store = store.clone();
        let result = page_store.connection.write_workers().spawn_blocking(move || {
            st3::profile::task("task coordination-counts-catch-up", || {
                page_store.advance_coordination_counts()
            })
        }).await;
        match result {
            Ok(Ok(true)) => return,
            Ok(Ok(false)) => tokio::time::sleep(Duration::from_millis(100)).await,
            error => {
                eprintln!("st3: coordination count bootstrap failed: {error:?}");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    }
}

/// One paced scan of the claims that predate the stop-deferral trigger, read on a reader
/// connection. Until it commits, retained-stop reads look those claims up directly.
async fn copy_stop_deferral_history(store: Arc<Store>) {
    loop {
        let scan_store = store.clone();
        let result = tokio::task::spawn_blocking(move || {
            st3::profile::task("task stop-deferral-history", || {
                scan_store.backfill_stop_deferral_history(|| {
                    std::thread::sleep(Duration::from_millis(10))
                })
            })
        })
        .await;
        match result {
            Ok(Ok(_)) => return,
            error => {
                eprintln!("st3: stop-deferral history copy failed: {error:?}");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    }
}

/// Commit notifications drive exact-delta maintenance after the API has bound.
/// Each bootstrap page yields the FIFO writer; malformed source stops the job and
/// leaves a durable incomplete state for doctor, rather than retrying a local defect.
fn spawn_publication_evidence(store: Arc<Store>) {
    let (changed, mut receiver) = tokio::sync::watch::channel(());
    let observer = store.observe_commits(move |connection| {
        if connection.is_autocommit() {
            changed.send_replace(());
        }
    });
    tokio::spawn(async move {
        let _observer = observer;
        loop {
            receiver.borrow_and_update();
            let read_store = store.clone();
            match tokio::task::spawn_blocking(move || read_store.publication_evidence_ready()).await
            {
                Ok(Ok(true)) => {
                    if receiver.changed().await.is_err() {
                        return;
                    }
                }
                Ok(Ok(false)) => {
                    let page_store = store.clone();
                    match page_store.connection.write_workers().spawn_blocking(move || {
                        page_store.advance_publication_evidence()
                    })
                    .await
                    {
                        Ok(Ok(_)) => tokio::task::yield_now().await,
                        error => {
                            eprintln!("st3: publication evidence bootstrap stopped: {error:?}");
                            return;
                        }
                    }
                }
                error => {
                    eprintln!("st3: publication evidence status failed: {error:?}");
                    return;
                }
            }
        }
    });
}

async fn migrate_event_payloads(store: Arc<Store>) {
    match st3::maintenance::migrate_event_payloads(store).await {
        Ok(report) if report.pending_at_start => {
            eprintln!(
                "st3: event payload migration completed: {} rows in {} chunks, {:.0} ms elapsed, {:.1} ms maximum chunk call",
                report.moved_rows, report.chunks, report.elapsed_ms, report.max_chunk_call_ms
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("st3: event payload migration stopped: {error:#}"),
    }
}

fn maintenance_contention(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(code, _)) if matches!(code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    })
}

/// Register before the first rowid deadline read. The callback performs no SQL and never
/// takes the writer: it only coalesces committed-write notifications. Deadline reads run on a
/// pooled reader, so unrelated commits add no expiry SQL to the writer path.
fn spawn_response_expiry(store: Arc<Store>) -> tokio::task::JoinHandle<()> {
    let (changed, receiver) = tokio::sync::watch::channel(());
    let observer = store.observe_commits(move |connection| {
        if connection.is_autocommit() {
            changed.send_replace(());
        }
    });
    tokio::spawn(async move {
        let _observer = observer;
        expire_completed_responses(store, receiver).await;
    })
}

async fn expire_completed_responses(
    store: Arc<Store>,
    mut changed: tokio::sync::watch::Receiver<()>,
) {
    let mut clock_deferred = false;
    let mut retry = Duration::from_millis(200);
    loop {
        // Subscribe before the rowid read. Consume the old generation before reading so a
        // commit between that read and the wait remains visible to changed().
        changed.borrow_and_update();
        let read_store = store.clone();
        let deadline =
            match tokio::task::spawn_blocking(move || read_store.next_idempotency_expiry()).await {
                Ok(Ok(deadline)) => deadline,
                Ok(Err(error)) if maintenance_contention(&error) => {
                    eprintln!(
                        "st3: response expiry read contention; retrying in {retry:?}: {error:#}"
                    );
                    tokio::time::sleep(retry).await;
                    retry = (retry * 2).min(Duration::from_secs(5));
                    continue;
                }
                error => {
                    eprintln!("st3: response expiry scheduler stopped: {error:?}");
                    return;
                }
            };
        let now = now_ms().min(i64::MAX as u128) as i64;
        if deadline.is_none_or(|deadline| deadline > now) {
            match deadline {
                None => {
                    if changed.changed().await.is_err() {
                        return;
                    }
                }
                Some(deadline) => {
                    // UTC stamps govern eligibility; the wait uses the monotonic clock. Every
                    // wake rechecks UTC, so a backward clock step cannot cause early expiry.
                    // A forward step may defer cleanup until this deadline or a committed
                    // deadline change; rows remain ineligible before their recorded UTC deadline.
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(deadline.saturating_sub(now) as u64)) => {},
                        result = changed.changed() => if result.is_err() {return;},
                    }
                }
            }
            continue;
        }
        let clock_store = store.clone();
        let clock_limit = match tokio::task::spawn_blocking(move || {
            clock_store.idempotency_clock_limit()
        })
        .await
        {
            Ok(Ok(limit)) => limit,
            Ok(Err(error)) if maintenance_contention(&error) => {
                eprintln!(
                    "st3: response expiry clock contention; retrying in {retry:?}: {error:#}"
                );
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(Duration::from_secs(5));
                continue;
            }
            error => {
                eprintln!("st3: response expiry clock read stopped: {error:?}");
                return;
            }
        };
        if clock_limit.is_none_or(|limit| deadline.unwrap() > limit) {
            if !clock_deferred {
                eprintln!(
                    "st3: response expiry deferred until committed claim time validates the deadline"
                );
                clock_deferred = true;
            }
            if changed.changed().await.is_err() {
                return;
            }
            continue;
        }
        clock_deferred = false;
        let clean_store = store.clone();
        match clean_store.connection.write_workers().spawn_blocking(move || {
            st3::profile::task("task expire-completed-responses", || {
                clean_store.cleanup_idempotency(now)
            })
        })
        .await
        {
            Ok(Ok(_)) => {
                retry = Duration::from_millis(200);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(Err(error)) if maintenance_contention(&error) => {
                eprintln!(
                    "st3: response expiry write contention; retrying in {retry:?}: {error:#}"
                );
                tokio::time::sleep(retry).await;
                retry = (retry * 2).min(Duration::from_secs(5));
            }
            error => {
                eprintln!("st3: response expiry cleanup stopped: {error:?}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod response_expiry_tests;

/// Fill the account limits projection after an upgrade, a page of claims at a time. Each page is
/// its own short writer transaction and the daemon answers between pages; the cursor is stored, so
/// a restart resumes. Once it is caught up this only checks, once a minute, for claims that
/// replication admitted and no append or projection pass has folded yet.
async fn catch_up_account_limits(store: Arc<Store>, notify: Arc<Notify>) {
    // A pool start that arrived while the projection was catching up failed with a retryable
    // error. Nothing else tells the reconciler that limits are now known, so wake it once.
    let mut announced = false;
    loop {
        let page_store = store.clone();
        let more = page_store.connection.write_workers().spawn_blocking(move || {
            st3::profile::task("task account-limits-catch-up", || {
                page_store.catch_up_account_limits(st3::store::LIMITS_CATCH_UP_PAGE)
            })
        })
        .await;
        if !announced && matches!(more, Ok(Ok(_))) {
            let ready_store = store.clone();
            let ready = tokio::task::spawn_blocking(move || ready_store.account_limits_ready())
                .await
                .ok()
                .and_then(|result| result.ok())
                .unwrap_or(false);
            if ready {
                announced = true;
                notify.notify_one();
            }
        }
        match more {
            Ok(Ok(true)) => tokio::time::sleep(Duration::from_millis(50)).await,
            _ => tokio::time::sleep(Duration::from_secs(60)).await,
        }
    }
}

#[cfg(test)]
mod limits_catch_up_wake_tests;

async fn trim_local_observations(store: Arc<Store>, observations: st3::config::ObservationsConfig) {
    const LOCAL_OBSERVATION_TRIM_INTERVAL: Duration = Duration::from_secs(60 * 60);
    const LOCAL_OBSERVATION_TRIM_CHUNK: usize = 5_000;
    let retention_ms = observations
        .retention_ms()
        .expect("the daemon validated its observation retention");
    loop {
        let observation_store = store.clone();
        let max_per_subject_kind = observations.max_per_subject_kind;
        let trimmed = observation_store.connection.write_workers().spawn_blocking(move || {
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
        if let Ok(Err(error)) = usage_store.connection.write_workers().spawn_blocking(move || {
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
        // The database target's sample: after the trims, so it counts what they freed.
        let size_store = store.clone();
        match size_store.connection.write_workers().spawn_blocking(move || size_store.record_database_size(now_ms())).await {
            Ok(Ok(size)) => st3::slo::note_database_size(&size),
            Ok(Err(error)) => eprintln!("st3: database size sample failed: {error:#}"),
            Err(error) => eprintln!("st3: database size sample stopped: {error}"),
        }
        tokio::time::sleep(LOCAL_OBSERVATION_TRIM_INTERVAL).await;
    }
}

/// Seal and verify checkpoints every ten minutes. The proof copies the store and replays it, so
/// it runs on a blocking thread, and a copy left by a crash is removed first.
async fn run_checkpoints(store: Arc<Store>, context: st3::store::CheckpointContext) {
    const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(10 * 60);
    // A proof copy is only ever useful to the proof that made it. A staged capture is kept: it
    // resumes where it stopped while its store-index cut still holds (#2492).
    if let Ok(entries) = std::fs::read_dir(&context.scratch) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("proof-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    loop {
        let store = store.clone();
        let context = st3::store::CheckpointContext {
            now_unix_ms: now_ms(),
            ..context.clone()
        };
        match store.connection.write_workers().spawn_blocking(move || {
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
mod claude_attachment_tests;

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;

#[cfg(test)]
mod terminal_ui_tests;

#[cfg(test)]
mod instance_uninstall_tests;
