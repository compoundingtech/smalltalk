//! The `st2` command tree: every clap declaration and nothing else.
//!
//! Split out of `main.rs` verbatim so the verb implementations and the declaration of the
//! surface they implement can be read separately. Items are `pub(crate)` because this is a
//! binary crate: nothing here is part of any public API.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "st2",
    version = st2::version::display_version(),
    about = "Harness-agnostic runner over a unified catalog+inbox folder"
)]
pub(crate) struct Cli {
    /// Catalog (or single-file fleet spec) to use. Defaults to $CATALOG, then
    /// ${XDG_STATE_HOME:-$HOME/.local/state}/st2/default/catalog.
    #[arg(long = "catalog", global = true, value_name = "PATH")]
    pub(crate) catalog_path: Option<PathBuf>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Discover and print every agent spec under a catalog+inbox folder.
    Ls {
        /// Legacy positional catalog/spec path. Prefer --catalog; defaults to $CATALOG, then the
        /// default st2 catalog.
        #[arg(conflicts_with = "catalog_path")]
        root: Option<PathBuf>,
    },
    /// Supervise a catalog+inbox folder: reconcile on a folder-watch + timer, keeping each agent's
    /// ptys running. With --once, do a single pass and exit.
    Up {
        /// Legacy positional catalog/spec path. Prefer --catalog; defaults to $CATALOG, then the
        /// default st2 catalog.
        #[arg(conflicts_with = "catalog_path")]
        root: Option<PathBuf>,
        /// Host to filter on (which agents this machine runs). Defaults to the local hostname.
        #[arg(long)]
        host: Option<String>,
        /// Do a single reconcile pass and exit, instead of looping.
        #[arg(long)]
        once: bool,
        /// Materialize every local agent's render block and exit without reconciling or spawning.
        #[arg(long, conflicts_with = "once")]
        materialize_only: bool,
        /// Limit materialization to one declared agent identity.
        #[arg(long)]
        agent: Option<String>,
        /// Select one exact local task. Use with --materialize-only to render only its owner, or
        /// with --once to render its owner and reconcile only that task.
        #[arg(long, conflicts_with = "agent")]
        task: Option<String>,
        /// Seconds between timer-driven reconcile passes when looping (folder changes reconcile
        /// immediately regardless).
        #[arg(long, default_value_t = 30)]
        interval: u64,
    },
    /// Native message bus: send/list/read/archive/reply over agents' `resources/inbox`.
    /// The stable wire format is a `<unix-ms>-<rand6>.md` Markdown file.
    #[command(subcommand)]
    Message(MessageCmd),
    /// Declared event streams: durable, bounded, idempotent ingress into an agent inbox.
    #[command(subcommand)]
    Event(EventCmd),
    /// Self-author declared event streams through the serialized catalog path.
    #[command(subcommand)]
    Stream(StreamCmd),
    /// Idempotent JSON request/reply transport for declared non-agent service principals.
    #[command(subcommand)]
    Request(RequestCmd),
    /// An agent's working-state context for lossless restart: read/write/append.
    #[command(subcommand)]
    Context(ContextCmd),
    /// An agent's declared Resource bindings (a named, exact URI a peer can resolve):
    /// ls/read/add/remove/rename.
    #[command(subcommand)]
    Resource(ResourceCmd),
    /// Install `st2 up` as a systemd-user service on headless Linux. macOS stays manual (TCC).
    /// Subcommands: install / status / uninstall.
    #[command(subcommand)]
    Service(ServiceCmd),
    /// Install and approve the embedded Claude Code channel plugin.
    #[command(subcommand)]
    ClaudeChannel(ClaudeChannelCmd),
    /// Explicit lifecycle-hook management. `up` and materialization only verify; they never install
    /// or refresh hooks.
    #[command(subcommand)]
    Hooks(HooksCmd),
    /// Provider-native harness drivers and read-only typed-block expansion.
    #[command(subcommand)]
    Driver(DriverCmd),
    /// The ding sidecar: watch an agent's `resources/inbox` and poke its pty (`[DING] …`) on each new
    /// message. Busy does not suppress delivery; only fresh dnd defers FIFO. A startup backlog is
    /// coalesced into one recovery notice. Long-running — st2 keeps it alive as a task alongside the
    /// agent. Exits when the target pty session is gone.
    /// `st2 ping` is an alias (the maintainer is renaming ding → ping, since dinging is the runner's
    /// job now); it is the exact same command.
    #[command(visible_alias = "ping")]
    Ding {
        /// The target pty session to poke (a `pty` session ref). Optional — defaults to `--identity`
        /// (an agent IS its pty, so the session to poke is the identity), so `st2 ding --identity X`
        /// is the common form.
        session: Option<String>,
        /// Whose inbox to watch — bus id or identity. Defaults to `$ST_AGENT`. Also the default poke
        /// target when no positional session is given.
        #[arg(long)]
        identity: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with `--identity`.
        #[arg(long = "agent-id", conflicts_with = "identity")]
        agent_id: Option<String>,
        /// Catalog root. Defaults to `$CATALOG`.
        #[arg(long, conflicts_with = "catalog_path")]
        root: Option<PathBuf>,
        /// Host used to resolve `<host>.<identity>` bus ids. Defaults to the local hostname.
        #[arg(long)]
        host: Option<String>,
        /// Poll/liveness cadence in milliseconds (folder changes poke immediately regardless).
        #[arg(long, default_value_t = 1000)]
        interval: u64,
    },
    /// Internal controlled Codex launch. Generated only for `deliver "app-server"` tasks.
    #[command(hide = true)]
    CodexAppServer {
        /// Exact agent bus identity that owns the controlled thread.
        #[arg(long)]
        identity: String,
        /// Exact reconciled PTY task identity for this runtime.
        #[arg(long)]
        runtime_id: String,
        /// Original structured Codex invocation, including its provider executable.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        codex_argv: Vec<String>,
    },
    /// Internal Claude MCP channel server started by Claude from its rendered project declaration.
    #[command(hide = true)]
    ClaudeMcp {
        #[arg(long)]
        identity: String,
    },
    /// Get or set an agent's presence status. No `--set` prints the status; no identity means yours
    /// (`$ST_AGENT`). Settable: offline | available | busy | away | dnd (`unknown` is derived).
    Status {
        /// Whose status — bus id or identity. Defaults to you (`--as` / `$ST_AGENT`).
        identity: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with the positional reference.
        #[arg(long = "id", conflicts_with = "identity")]
        agent_id: Option<String>,
        /// Set your status to this state instead of printing it.
        #[arg(long = "set")]
        set: Option<String>,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Set or clear an agent's human-facing name without changing stable identity.
    Rename(PresentationArgs),
    /// Set or clear an agent's enduring responsibility description.
    Describe(PresentationArgs),
    /// Transactionally publish one canonical Agent Spec into the live catalog.
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Canonical declaration snapshots and crash-recoverable whole-catalog application.
    #[command(subcommand)]
    Catalog(CatalogCmd),
    /// Explicit teardown: kill every live task of this host's catalog agents. The ONLY thing that ends
    /// tasks (stopping/crashing st2 never does). Idempotent.
    Down {
        /// Optional positional catalog/spec path. Prefer --catalog; defaults to $CATALOG, then the
        /// standard st2 catalog.
        #[arg(conflicts_with = "catalog_path")]
        root: Option<PathBuf>,
        /// Host to tear down. Defaults to the local hostname.
        #[arg(long)]
        host: Option<String>,
    },
    /// Print shell exports for a catalog's bus — `eval "$(st2 env --catalog <catalog>)"` sets `CATALOG`/
    /// `ST_ROOT`/`PTY_ROOT` so native bus-aware tools target the catalog.
    Env {
        /// Optional positional catalog path. Prefer --catalog; defaults to $CATALOG, then the
        /// standard st2 catalog.
        #[arg(conflicts_with = "catalog_path")]
        root: Option<PathBuf>,
    },
    /// Explicitly pre-trust workspaces in the ambient Claude and Codex configs. This is an operator
    /// utility for harnesses that use those ambient configs; `st2 up` never calls it automatically.
    /// Account-selecting commands should instead declare trust in the selected harness invocation.
    Pretrust {
        /// Workspace directories to mark trusted.
        #[arg(required = true)]
        dirs: Vec<PathBuf>,
    },
    /// Run an st2-spec eval end to end: copy the fixture, boot the team + judges, deliver the
    /// kickoff, wait for the sup's confirmation, run the judges → verdict. `st2 eval ./cells/<name>/`.
    Eval {
        /// The eval folder (or its `.kdl` spec file).
        folder: PathBuf,
        /// Host. Defaults to the local hostname.
        #[arg(long)]
        host: Option<String>,
        /// Preserve the run's temp catalog instead of deleting it — for inspecting the worker repo
        /// (`base..HEAD`), the judge outputs, and the bus after the run (e.g. a gate reproduction).
        /// Seats are still torn down (no leaks). Also honored via `ST2_EVAL_KEEP`.
        #[arg(long)]
        keep: bool,
        /// Emit the existing eval report as JSON without changing exit semantics.
        #[arg(long)]
        json: bool,
    },
    /// Run `pty` against this catalog's bus with the env auto-set, so pty subcommands and the
    /// interactive UI work without `eval "$(st2 env --catalog <catalog>)"` first. Catalog selection follows
    /// `--catalog`, `$CATALOG`, then the default st2 catalog. `CATALOG`/`ST_ROOT`/`PTY_ROOT` are
    /// exported for the child exactly as `st2 env` would. No arguments launches the interactive pty
    /// UI.
    Pty {
        /// Arguments passed through to `pty` verbatim (e.g. `ls`, `peek <session>`). None → the UI.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Drop into `$SHELL` with this catalog's bus env set (`CATALOG`/`ST_ROOT`/`PTY_ROOT`), so `pty`,
    /// bus-aware tools target the catalog for the whole session without `eval "$(st2 env …)"`.
    /// The general form of `st2 pty`. Catalog selection follows `--catalog`, `$CATALOG`, then the
    /// default st2 catalog; extra args go to the shell (e.g. `st2 shell -c "pty ls"`).
    Shell {
        /// Arguments passed through to `$SHELL` verbatim. None → an interactive shell.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Validate a rendered catalog against the runner contract (spec fields, folder layout, paths)
    /// so any renderer can confirm it hit the spec before running. One line per issue with
    /// a stable code; exits non-zero on any ERROR (`--strict` also fails on warnings). `--json` emits
    /// machine output for a renderer's build gate.
    Validate {
        /// Legacy positional catalog path. Prefer --catalog; defaults to $CATALOG, then the default
        /// st2 catalog.
        #[arg(conflicts_with = "catalog_path")]
        root: Option<PathBuf>,
        /// Host whose external workspace/task paths should be checked. Structural checks always
        /// cover the whole catalog. Defaults to the local hostname.
        #[arg(long)]
        host: Option<String>,
        /// Validate this one unpublished canonical Agent Spec as an overlay on the live catalog.
        #[arg(long, value_name = "FILE")]
        candidate: Option<PathBuf>,
        /// Fail (non-zero exit) on warnings too, not just errors.
        #[arg(long)]
        strict: bool,
        /// Emit the report as JSON instead of human-readable lines.
        #[arg(long)]
        json: bool,
    },
    /// Health check for a catalog: active agents alive, suspended agents not live, and retired
    /// agents fully absent. Exits non-zero on problems.
    Doctor {
        /// Legacy positional catalog path. Prefer --catalog; defaults to $CATALOG, then the default
        /// st2 catalog.
        #[arg(conflicts_with = "catalog_path")]
        root: Option<PathBuf>,
        /// Host to check. Defaults to the local hostname.
        #[arg(long)]
        host: Option<String>,
        /// Require a live long-running `st2 up` host lock. Omit for manual/--once operation.
        #[arg(long)]
        require_supervisor: bool,
    },
    /// List every agent in the catalog with presence and retirement state. `--json [--enrich]` is
    /// the stable machine-readable roster.
    Agents {
        /// The catalog folder (like `st2 ls`/`up`). Falls back to `--root`/`$CATALOG`.
        #[arg(conflicts_with = "catalog_path")]
        catalog: Option<PathBuf>,
        /// Only agents whose effective status matches (offline|available|busy|away|dnd|unknown).
        #[arg(long = "status")]
        status: Option<String>,
        /// Select one exact Agent Spec by its fully qualified `<host>.<identity>`.
        #[arg(long, value_name = "HOST.IDENTITY")]
        identity: Option<String>,
        /// Select one exact subject by its immutable agent ID (R24).
        #[arg(long = "id", conflicts_with = "identity")]
        agent_id: Option<String>,
        /// Machine-readable JSON array, including retirement and declared Resource bindings.
        #[arg(long)]
        json: bool,
        /// With `--json`, add `lastActivity` + `inbox` count per agent.
        #[arg(long)]
        enrich: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Emit one fail-closed desired-task/runtime diagnostic snapshot. This is
    /// read-only observation, not reconciliation or cutover authority.
    Tasks {
        /// Host whose desired tasks and runtime generations to inspect. Defaults to this host.
        #[arg(long)]
        host: Option<String>,
        /// Emit the versioned machine-readable envelope. Required in v1.
        #[arg(long)]
        json: bool,
    },
    /// Clear one task's park after fixing what crash-looped it. A task parked by its `restart{}`
    /// policy (mode=fail) stays parked for the rest of the supervisor run, and this is its per-task
    /// exit: the running supervisor relaunches exactly this task on its next pass, leaving every
    /// other task on the host untouched. `st2 tasks --json` reports which tasks are parked.
    Unpark {
        /// The parked task's runtime id, exactly as `st2 tasks --json` reports it.
        task: String,
        /// Host whose selected-catalog supervisor should grant the request. Defaults to this host.
        #[arg(long)]
        host: Option<String>,
    },
    /// Print a shell completion script for `st2` to stdout (`st2 completions <bash|zsh|fish|…>`).
    /// Generated from the live command tree, so it never drifts from the actual flags.
    Completions {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
pub(crate) enum DriverCmd {
    /// Print one typed driver block as plain Agent Spec KDL without running it.
    Expand {
        /// KDL declaration that contains the typed driver block.
        spec: PathBuf,
        /// Select one local or fully qualified identity when the file contains multiple agents.
        #[arg(long)]
        agent: Option<String>,
        /// Host fallback when neither the declaration nor its catalog path supplies one.
        #[arg(long)]
        host: Option<String>,
    },
    /// Run the existing controlled Codex app-server path.
    Codex {
        #[arg(long)]
        identity: String,
        #[arg(long)]
        runtime_id: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Run the Claude session-owned MCP server over stdio.
    ClaudeMcp {
        #[arg(long)]
        identity: Option<String>,
    },
    /// Deprecated name for the Claude MCP server.
    // Keep this hidden command until no rendered configuration uses the old name.
    #[command(hide = true)]
    Claude {
        #[arg(long)]
        identity: String,
    },
    /// Run Claude under the session-owned presence wrapper.
    ClaudeSession {
        #[arg(long)]
        identity: String,
        #[arg(long)]
        runtime_id: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Apply one Claude hook event (payload on stdin) to observed harness state.
    ClaudeObserve {
        #[arg(long)]
        identity: String,
        /// The wrapper's runtime/task ID; the record's pty session. Defaults to the identity.
        #[arg(long)]
        runtime_id: Option<String>,
        /// The Claude hook event name, e.g. `Stop` or `PermissionRequest`.
        #[arg(long)]
        event: String,
    },
    /// Tee Claude's status-line payload (stdin JSON) into harness context, then chain to the
    /// operator's own renderer.
    ClaudeStatusline {
        #[arg(long)]
        identity: String,
    },
    /// Run pi under the session-owned presence wrapper.
    PiSession {
        #[arg(long)]
        identity: String,
        #[arg(long)]
        runtime_id: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Run the pi native message channel over stdio, owned by the shipped pi extension.
    PiChannel {
        #[arg(long)]
        identity: String,
    },
    /// Run omp under the session-owned presence wrapper with a hard version gate.
    OmpSession {
        #[arg(long)]
        identity: String,
        #[arg(long)]
        runtime_id: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
    /// Run the omp native message channel over stdio, owned by the shipped omp extension.
    OmpChannel {
        #[arg(long)]
        identity: String,
    },
    /// Run OpenCode under the session-owned wrapper: presence, observed harness state, and native
    /// server delivery over the wrapper-allocated local port.
    OpencodeSession {
        #[arg(long)]
        identity: String,
        #[arg(long)]
        runtime_id: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum AgentCmd {
    /// Author reversible whole-agent lifecycle intent in one canonical KDL declaration.
    DesiredState {
        /// Exact bus identity, or a bare stable identity only when unique — or the desired state
        /// when `--id` names the subject. Authoring selects the declaration, never the address.
        #[arg(value_name = "IDENTITY_OR_STATE")]
        first: Option<String>,
        /// The desired state, when the first positional is the agent reference.
        #[arg(value_name = "STATE")]
        second: Option<String>,
        /// Exact immutable agent ID (R24). The first positional is then the desired state.
        ///
        /// Both positionals stay optional so the exact-ID form can shift them; clap refuses a
        /// non-required positional ahead of a required one, which is why the state is validated in
        /// the handler rather than by a positional `value_parser`.
        #[arg(long = "id", conflicts_with = "second")]
        agent_id: Option<String>,
        /// Required rationale for suspended/retired; forbidden for running.
        #[arg(long)]
        reason: Option<String>,
        /// Assert the ownership marker that owns this declaration, e.g. `nix`.
        ///
        /// A declaration carrying `meta { managed-by "nix" }` refuses ordinary authoring,
        /// because the Nix projection is the writer of those bytes. This is how that
        /// projection authors lifecycle on its own declaration — the transition it cannot
        /// express in its own source, because the source change being projected is the
        /// seat's removal. The assertion is admitted only when it names exactly the one
        /// marker the declaration carries.
        #[arg(long = "managed-by", value_name = "MARKER")]
        managed_by: Option<String>,
        /// Host used only to resolve declarations whose host is omitted.
        #[arg(long)]
        host: Option<String>,
        /// Emit a stable JSON authoring receipt.
        #[arg(long)]
        json: bool,
    },
    /// Assign or clear an agent's mutable address — one atomic address-book cutover with no
    /// alias, redirect, or rename history. `--clear` restores the positional identity fallback.
    Address(PresentationArgs),
    /// Compute the authoritative digest bound by `agent publish --input-sha256`.
    Digest {
        /// A canonical KDL file containing exactly one top-level `agent` node.
        #[arg(
            long,
            value_name = "FILE",
            required_unless_present = "bundle",
            conflicts_with = "bundle"
        )]
        spec: Option<PathBuf>,
        /// A create-only directory whose root contains exactly one canonical `agent.kdl`.
        #[arg(
            long,
            value_name = "DIR",
            required_unless_present = "spec",
            conflicts_with = "spec"
        )]
        bundle: Option<PathBuf>,
        /// Emit the typed source-digest receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Publish exactly one explicit-host, explicit-identity agent under a catalog-wide CAS lock.
    Publish {
        /// A canonical KDL file containing exactly one top-level `agent` node.
        #[arg(
            long,
            value_name = "FILE",
            required_unless_present = "bundle",
            conflicts_with = "bundle"
        )]
        spec: Option<PathBuf>,
        /// A create-only directory whose root contains exactly one canonical `agent.kdl`.
        #[arg(
            long,
            value_name = "DIR",
            required_unless_present = "spec",
            conflicts_with = "spec"
        )]
        bundle: Option<PathBuf>,
        /// Create only. An identical existing agent.kdl is reported as `unchanged`.
        #[arg(
            long,
            required_unless_present = "expect_sha256",
            conflicts_with = "expect_sha256"
        )]
        expect_absent: bool,
        /// Replace only when the current agent.kdl has this lowercase SHA-256.
        #[arg(
            long,
            value_name = "HEX",
            required_unless_present = "expect_absent",
            conflicts_with = "expect_absent"
        )]
        expect_sha256: Option<String>,
        /// SHA-256 returned by `st2 agent digest` for the exact source capability.
        #[arg(long, value_name = "HEX")]
        input_sha256: String,
        /// Assert the ownership marker that owns the declaration being replaced, e.g. `nix`.
        ///
        /// A declaration carrying `meta { managed-by "nix" }` refuses an unasserted
        /// replacement, because the Nix projection is the writer of those bytes. The
        /// assertion is admitted only when it names exactly the one marker the incumbent
        /// carries. Create-only publication has no incumbent and needs no assertion.
        #[arg(long = "managed-by", value_name = "MARKER")]
        managed_by: Option<String>,
        /// Emit the typed publication result as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum CatalogCmd {
    /// Emit one fail-closed declaration graph plus runtime observation envelope.
    Graph {
        /// Host used to resolve declarations with no host and host-local runtime facts.
        #[arg(long)]
        host: Option<String>,
        /// Emit the versioned machine-readable envelope. Required in v1.
        #[arg(long)]
        json: bool,
    },
    /// Compute the authoritative digest bound by `catalog apply --input-sha256`.
    Digest {
        /// Complete prepared declaration directory. Runtime state and control paths are rejected.
        #[arg(long, value_name = "DIR")]
        prepared: PathBuf,
        /// Emit the typed source-digest receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compare one prepared declaration directory with the coherent live catalog without writing.
    Diff {
        /// Complete prepared declaration directory. Runtime state and control paths are rejected.
        #[arg(long, value_name = "DIR")]
        prepared: PathBuf,
        /// Expected canonical declaration-root SHA-256 of the live catalog.
        #[arg(long, value_name = "HEX")]
        expect_sha256: String,
        /// Emit the versioned semantic-diff receipt. Required in v1.
        #[arg(long)]
        json: bool,
    },
    /// Publish a complete prepared declaration directory as one absent catalog.
    Bootstrap {
        /// Complete prepared declaration directory. Runtime state and control paths are rejected.
        #[arg(long, value_name = "DIR")]
        prepared: PathBuf,
        /// Root SHA-256 of the exact prepared projection being published.
        #[arg(long, value_name = "HEX")]
        input_sha256: String,
        /// Emit the typed bootstrap receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Capture the coherent declaration plane into a create-only canonical directory.
    Snapshot {
        /// Destination directory. It must be outside the live catalog.
        #[arg(long, value_name = "DIR")]
        output: PathBuf,
        /// Hash and capture the declaration plane without parsing it. The captured directory
        /// remains unvalidated and is suitable only as an exact-byte CAS preimage.
        #[arg(long)]
        raw_preimage: bool,
        /// Emit the typed snapshot receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Apply a complete canonical declaration directory under declaration-root CAS.
    Apply {
        /// Complete prepared declaration directory. Runtime state and control paths are rejected.
        #[arg(
            long,
            value_name = "DIR",
            required_unless_present = "resume",
            conflicts_with = "resume"
        )]
        prepared: Option<PathBuf>,
        /// Root SHA-256 of the exact prepared projection being applied.
        #[arg(
            long,
            value_name = "HEX",
            required_unless_present = "resume",
            conflicts_with = "resume"
        )]
        input_sha256: Option<String>,
        /// Expected canonical declaration-root SHA-256 of the live catalog.
        #[arg(
            long,
            value_name = "HEX",
            required_unless_present = "resume",
            conflicts_with = "resume"
        )]
        expect_sha256: Option<String>,
        /// Match the current declaration plane without parsing it. The prepared catalog is still
        /// fully validated; use this mode only when the current parser cannot admit the preimage.
        #[arg(long, conflicts_with = "resume")]
        raw_preimage: bool,
        /// Resume the durable incomplete marker and internal stage without the original source.
        #[arg(long, conflicts_with_all = ["prepared", "input_sha256", "expect_sha256"])]
        resume: bool,
        /// Emit the typed application receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Move retired, runtime-free identities out of the live catalog into `.st2/archive`, leaving a
    /// tombstone row in `st2 catalog graph --json`. An archived spec is not discoverable.
    Archive {
        /// Identity to archive, repeatable. Every named identity must be eligible or nothing moves.
        #[arg(
            long,
            value_name = "IDENTITY",
            required_unless_present = "all_retired",
            conflicts_with = "all_retired"
        )]
        identity: Vec<String>,
        /// Archive every eligible retired identity of the selected host. Ineligible ones are
        /// reported and skipped.
        #[arg(long)]
        all_retired: bool,
        /// Host whose identities are archived. Defaults to this host; another host's runtime
        /// records are not observable from here, so only the local host is eligible.
        #[arg(long)]
        host: Option<String>,
        /// Decide eligibility and print the plan without moving anything.
        #[arg(long)]
        dry_run: bool,
        /// Emit the typed archive receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Move one archived identity back into the live catalog. The exact reverse of `archive`.
    Unarchive {
        /// Archived identity to restore.
        identity: String,
        /// Host the identity was archived under. Defaults to this host.
        #[arg(long)]
        host: Option<String>,
        /// Emit the typed restoration receipt as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Shared context for message subcommands: where the catalog is, who "I" am, and the local host.
/// Defaults come from the same env st2 sets on every task it spawns (`$CATALOG`, `$ST_AGENT`), so a
/// running agent needs no flags.
#[derive(Args)]
pub(crate) struct MsgCtx {
    /// Legacy catalog/bus root override. Prefer global `--catalog`; defaults to `$CATALOG`, then the
    /// default st2 catalog.
    #[arg(long, conflicts_with = "catalog_path")]
    pub(crate) root: Option<PathBuf>,
    /// The acting identity — who the message is `from` / whose inbox is "mine". An ordinary
    /// address reference, unlike `$ST_AGENT`, which carries the exact agent ID; the two are
    /// different strings once a subject declares an explicit `address`. Defaults to `$ST_AGENT`.
    #[arg(long = "as")]
    pub(crate) as_id: Option<String>,
    /// Host used to resolve `<host>.<identity>` bus ids. Defaults to the local hostname.
    #[arg(long)]
    pub(crate) host: Option<String>,
}

/// `[<reference>] <value>` plus the mutually exclusive exact-ID form.
///
/// `--id` takes the agent off the positional list, so the first positional is then the value —
/// the same `[identity] <thing>` convention `st2 message read` and `st2 resource read` already
/// use, and the reason clap's exclusion is expressed against the second positional.
#[derive(Args)]
pub(crate) struct PresentationArgs {
    /// Exact bus identity, or a bare stable identity only when unique in the selected catalog —
    /// or the new value when `--id` names the subject. Authoring selects the declaration, never
    /// the address.
    #[arg(value_name = "IDENTITY_OR_TEXT")]
    pub(crate) first: Option<String>,
    /// The new value, when the first positional is the agent reference.
    #[arg(value_name = "TEXT")]
    pub(crate) second: Option<String>,
    /// Exact immutable agent ID (R24). The first positional is then the new value.
    #[arg(long = "id", conflicts_with = "second")]
    pub(crate) agent_id: Option<String>,
    /// Remove the optional field.
    #[arg(long)]
    pub(crate) clear: bool,
    /// Emit a stable JSON receipt or classified refusal.
    #[arg(long)]
    pub(crate) json: bool,
    /// Host used only to resolve declarations whose host is omitted.
    #[arg(long)]
    pub(crate) host: Option<String>,
}

impl PresentationArgs {
    /// The selected subject and the requested value, where `None` is the cleared representation.
    pub(crate) fn selection(self) -> Result<(st2::identity::AgentSelector, Option<String>, bool, Option<String>)>
    {
        let (selector, value) = match self.agent_id {
            Some(id) => (st2::identity::AgentSelector::Id(id), self.first),
            None => (
                st2::identity::AgentSelector::Address(
                    self.first
                        .context("no agent selected: pass an agent reference or the `--id` form")?,
                ),
                self.second,
            ),
        };
        anyhow::ensure!(
            !(self.clear && value.is_some()),
            "--clear removes the field and takes no value"
        );
        anyhow::ensure!(
            self.clear || value.is_some(),
            "a value is required unless --clear"
        );
        Ok((
            selector,
            if self.clear { None } else { value },
            self.json,
            self.host,
        ))
    }
}

#[derive(Subcommand)]
pub(crate) enum ServiceCmd {
    /// Write the `st2.service` systemd-user unit, enable it (start on boot), and start it now.
    /// Idempotent — safe to re-run. The unit runs `st2 up --catalog <catalog>`; agents spawn in sibling
    /// scopes, so a service restart never cascades to them.
    Install {
        /// Legacy positional catalog/spec path for `st2 up`. Prefer --catalog; defaults to
        /// `$CATALOG`, then the default st2 catalog. It must exist at install time.
        #[arg(conflicts_with = "catalog_path")]
        catalog: Option<PathBuf>,
        /// Bake `--host <h>` into the unit. Omit to let `st2 up` auto-detect the hostname at runtime.
        #[arg(long)]
        host: Option<String>,
        /// Machine-local pty registry to export as PTY_ROOT in the unit. Omit to use
        /// `<catalog>/pty`. Useful when adopting live sessions from a legacy runner.
        #[arg(long)]
        pty_root: Option<PathBuf>,
        /// Supervisor memory ceiling (MiB). The agents live in sibling scopes and are NOT bounded.
        #[arg(long = "memory-max-mb", default_value_t = st2::service::DEFAULT_MEMORY_MAX_MB)]
        memory_max_mb: u64,
    },
    /// Show the `st2.service` systemd status.
    Status,
    /// Stop, disable, and remove the `st2.service` unit. Idempotent.
    Uninstall,
}

#[derive(Subcommand)]
pub(crate) enum ClaudeChannelCmd {
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
    /// Remove only the st2-owned machine policy fragment.
    #[command(hide = true)]
    UninstallPolicy,
}

#[derive(Subcommand)]
pub(crate) enum HooksCmd {
    /// Atomically publish this binary's immutable hook set and select it with a receipt.
    Install {
        /// Select this binary's exact hook set even when it is older or cannot be ordered.
        #[arg(long)]
        replace: bool,
        /// Deprecated compatibility alias for `--replace`.
        #[arg(long, hide = true)]
        allow_downgrade: bool,
    },
    /// Read-only verification of the selected receipt and every embedded hook byte.
    Verify,
    /// Verify this binary's immutable hook set without requiring it to be selected.
    VerifyOwn,
}

#[derive(Subcommand)]
pub(crate) enum ResourceCmd {
    /// List an agent's declared Resource bindings. Defaults to your own.
    Ls {
        /// Whose declaration to read — bus id or bare identity. Defaults to you (`$ST_AGENT`).
        identity: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with the positional reference.
        #[arg(long = "id", conflicts_with = "identity")]
        agent_id: Option<String>,
        /// Emit the bindings as a JSON array.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Read one declared binding. With a leading identity, from that agent; otherwise your own.
    Read {
        first: String,
        second: Option<String>,
        /// Exact immutable agent ID (R24); `first` is then the binding name.
        #[arg(long = "id", conflicts_with = "second")]
        agent_id: Option<String>,
        /// Emit the binding as a JSON object.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Ask the resident profile runtime to observe one binding now and wait for exact evidence.
    Refresh {
        /// Binding name, or an agent selector when followed by a binding name.
        first: String,
        /// Binding name when the first positional selects the agent.
        second: Option<String>,
        /// Exact target agent; defaults to --as / $ST_AGENT.
        #[arg(long, conflicts_with = "second")]
        agent: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with `--agent`.
        #[arg(long = "agent-id", conflicts_with_all = ["second", "agent"])]
        agent_id: Option<String>,
        /// Client-only wait bound in seconds. Expiry never cancels or retracts queued demand.
        #[arg(long, default_value_t = 30)]
        wait: u64,
        /// Emit the stable receipt (or timeout envelope) as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Declare a Resource binding, or prove the identical binding already exists.
    Add {
        /// The agent-local binding name.
        name: String,
        /// The exact absolute URI this binding names (any `scheme:` — the identity is verbatim).
        #[arg(long)]
        uri: String,
        /// Why this reference belongs in the declaration.
        #[arg(long)]
        reason: String,
        /// Preserve the binding as no longer active for this agent, and say why.
        #[arg(long = "inactive-reason", value_name = "TEXT")]
        inactive_reason: Option<String>,
        /// Profile-specific observation selector as JSON.
        #[arg(long = "selector-json", value_name = "JSON")]
        selector_json: Option<String>,
        /// Exact target agent; defaults to --as / $ST_AGENT.
        #[arg(long)]
        agent: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with `--agent`.
        #[arg(long = "agent-id", conflicts_with = "agent")]
        agent_id: Option<String>,
        /// Emit a stable JSON receipt.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Remove one declared binding, or prove it is already absent.
    Remove {
        /// The agent-local binding name.
        name: String,
        /// Exact target agent; defaults to --as / $ST_AGENT.
        #[arg(long)]
        agent: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with `--agent`.
        #[arg(long = "agent-id", conflicts_with = "agent")]
        agent_id: Option<String>,
        /// Emit a stable JSON receipt.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Rename one declared binding's agent-local label, keeping its uri and reasons.
    Rename {
        /// The current binding name.
        old: String,
        /// The new binding name.
        new: String,
        /// Exact target agent; defaults to --as / $ST_AGENT.
        #[arg(long)]
        agent: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with `--agent`.
        #[arg(long = "agent-id", conflicts_with = "agent")]
        agent_id: Option<String>,
        /// Emit a stable JSON receipt.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
}

#[derive(Subcommand)]
pub(crate) enum ContextCmd {
    /// Print an agent's context. Default = `now.md` (working state); `--decisions` the log; `--full` both.
    Read {
        /// Whose context — bus id or identity. Defaults to you (`$ST_AGENT`).
        identity: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with the positional reference.
        #[arg(long = "id", conflicts_with = "identity")]
        agent_id: Option<String>,
        /// Print the decision log instead of the working state.
        #[arg(long)]
        decisions: bool,
        /// Print the working state and the decision log.
        #[arg(long)]
        full: bool,
        /// Print `now.md` only when it is newer than this many seconds.
        #[arg(long, value_name = "SECONDS")]
        fresh_within: Option<u64>,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Overwrite an agent's working state (`now.md`) from stdin.
    Write {
        identity: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with the positional reference.
        #[arg(long = "id", conflicts_with = "identity")]
        agent_id: Option<String>,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Append a single decision (with its reasoning) to the log.
    Append {
        identity: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with the positional reference.
        #[arg(long = "id", conflicts_with = "identity")]
        agent_id: Option<String>,
        /// The decision — a single line.
        #[arg(long)]
        decision: String,
        /// Why — a single line.
        #[arg(long)]
        why: String,
        #[command(flatten)]
        ctx: MsgCtx,
    },
}

#[derive(Subcommand)]
pub(crate) enum MessageCmd {
    /// Send a new message to a recipient's inbox.
    Send {
        /// Recipient: a bus address (`<host>.<address>`) or a bare address in the catalog.
        #[arg(required_unless_present = "to_id", conflicts_with = "to_id")]
        to: Option<String>,
        /// Recipient by exact immutable agent ID (R24). Mutually exclusive with the positional.
        #[arg(long = "to-id")]
        to_id: Option<String>,
        /// The message body. Read from stdin when omitted.
        #[arg(short = 'm', long = "message")]
        body: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long = "in-reply-to")]
        in_reply_to: Option<String>,
        /// Comma-separated tags.
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Reuse one sender-owned operation result across exact retries.
        #[arg(long = "idempotency-key")]
        idempotency_key: Option<String>,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Reply to a message in your inbox — recipient and threading are derived from it.
    Reply {
        /// The message filename in your inbox to reply to.
        filename: String,
        /// The reply body. Read from stdin when omitted.
        #[arg(short = 'm', long = "message")]
        body: Option<String>,
        /// Override the subject (defaults to `re: <original subject>`).
        #[arg(long)]
        subject: Option<String>,
        /// Reuse one sender-owned operation result across exact retries.
        #[arg(long = "idempotency-key")]
        idempotency_key: Option<String>,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// List an inbox (or `--archive`), sorted by send time. Defaults to your own.
    Ls {
        /// Whose inbox — bus id or identity. Defaults to you (`--as` / `$ST_AGENT`).
        identity: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with the positional reference.
        #[arg(long = "id", conflicts_with = "identity")]
        agent_id: Option<String>,
        /// List the archive instead of the inbox.
        #[arg(long)]
        archive: bool,
        /// Recovery-only: list the raw flat `<root>/<identity>` box without catalog resolution.
        #[arg(long)]
        orphan: bool,
        /// Print only the message count.
        #[arg(long)]
        count: bool,
        /// Include full message bodies in JSON output (opt-in; default shape is unchanged).
        #[arg(long)]
        include_body: bool,
        /// Show only messages from this sender.
        #[arg(long = "from")]
        from: Option<String>,
        /// Show only messages sent after this unix-millisecond timestamp.
        #[arg(long)]
        since: Option<u64>,
        /// Machine-readable JSON array.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// List sender-owned durable message history. Defaults to your own index.
    Sent {
        /// Whose sent index — bus id or identity. Defaults to you (`--as` / `$ST_AGENT`).
        identity: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with the positional reference.
        #[arg(long = "id", conflicts_with = "identity")]
        agent_id: Option<String>,
        /// Print only the indexed message count. Refuses unavailable or partial coverage.
        #[arg(long)]
        count: bool,
        /// Include full message bodies in JSON output.
        #[arg(long)]
        include_body: bool,
        /// Show only messages addressed to this canonical recipient.
        #[arg(long = "to")]
        to: Option<String>,
        /// Show only messages sent after this unix-millisecond timestamp.
        #[arg(long)]
        since: Option<u64>,
        /// Machine-readable coverage envelope and rows.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Read one message. With a leading identity, read from that agent's box; otherwise your own.
    Read {
        /// Either the message filename, or an identity followed by a filename.
        first: String,
        /// The message filename (when `first` is an identity).
        second: Option<String>,
        /// Exact immutable agent ID (R24) owning the box; `first` is then the filename.
        #[arg(long = "id", conflicts_with = "second")]
        agent_id: Option<String>,
        /// Read from the archive instead of the inbox.
        #[arg(long)]
        archive: bool,
        /// Print the file verbatim (frontmatter + body), not a formatted view.
        #[arg(long)]
        raw: bool,
        /// Machine-readable JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Archive one message (inbox → archive). Defaults to your own inbox.
    Archive {
        /// Either the message filename, or an identity followed by a filename.
        first: String,
        /// The message filename (when `first` is an identity).
        second: Option<String>,
        /// Exact immutable agent ID (R24) owning the box; `first` is then the filename.
        #[arg(long = "id", conflicts_with = "second")]
        agent_id: Option<String>,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Show a message's thread — the message + everything replying to it (transitively), across the
    /// catalog. `--tree` indents by reply depth; otherwise flat chronological.
    Thread {
        /// Either the message filename, or an identity followed by a filename.
        first: String,
        /// The message filename (when `first` is an identity).
        second: Option<String>,
        /// Indented hierarchical output instead of flat chronological.
        #[arg(long)]
        tree: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
}

#[derive(Subcommand)]
pub(crate) enum EventCmd {
    /// Emit one producer-identified event into a declared agent stream.
    Emit {
        /// Owning agent: a bus address (`<host>.<address>`) or a bare local address.
        #[arg(required_unless_present = "recipient_id", conflicts_with = "recipient_id")]
        recipient: Option<String>,
        /// Owning agent by exact immutable agent ID (R24).
        #[arg(long = "recipient-id")]
        recipient_id: Option<String>,
        /// Declared stream name.
        #[arg(long)]
        stream: String,
        /// Stable producer-supplied event identity.
        #[arg(long = "event-id")]
        event_id: String,
        /// Producer grouping key used by --supersede.
        #[arg(long)]
        key: Option<String>,
        /// Archive the unread predecessor for the same key, or the stream-wide head without --key.
        #[arg(long)]
        supersede: bool,
        /// One-line wake-time summary.
        #[arg(long)]
        subject: Option<String>,
        /// Event body. Read from stdin when omitted.
        #[arg(short = 'm', long = "message")]
        body: Option<String>,
        /// Emit the stable machine receipt.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
}

#[derive(Subcommand)]
pub(crate) enum StreamCmd {
    /// Add a stream to your declaration, optionally with a supervised adapter launch.
    Add {
        name: String,
        /// Exact target agent; defaults to --as / $ST_AGENT.
        #[arg(long)]
        agent: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with `--agent`.
        #[arg(long = "agent-id", conflicts_with = "agent")]
        agent_id: Option<String>,
        /// Adapter command run under `sh -c`; omit both launch forms for external ingress.
        #[arg(long, conflicts_with = "adapter_argv")]
        command: Option<String>,
        /// Direct adapter argv after `--`. Element 0 is the program; values are preserved exactly.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        adapter_argv: Vec<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Remove a stream from your declaration.
    Rm {
        name: String,
        /// Exact target agent; defaults to --as / $ST_AGENT.
        #[arg(long)]
        agent: Option<String>,
        /// Exact immutable agent ID (R24). Mutually exclusive with `--agent`.
        #[arg(long = "agent-id", conflicts_with = "agent")]
        agent_id: Option<String>,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
}

#[derive(Subcommand)]
pub(crate) enum RequestCmd {
    /// Publish one idempotent JSON request from a declared service principal to an agent.
    Send {
        /// Recipient agent: a bus id (`<host>.<identity>`) or a local bare identity.
        to: String,
        #[arg(long = "idempotency-key")]
        idempotency_key: String,
        /// Typed request tag as `key=value` (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// JSON body. Read from stdin when omitted.
        #[arg(short = 'm', long = "message")]
        body: Option<String>,
        /// Emit the machine receipt as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Decode one typed request from an agent's inbox.
    Read {
        request_filename: String,
        /// Emit the request envelope as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Reply once to a typed request in an agent's inbox.
    Reply {
        request_filename: String,
        /// Typed reply tag as `key=value` (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// JSON body. Read from stdin when omitted.
        #[arg(short = 'm', long = "message")]
        body: Option<String>,
        /// Emit the machine receipt as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
    /// Observe the typed reply for one previously published request.
    Status {
        #[arg(long = "idempotency-key")]
        idempotency_key: String,
        /// Emit the tagged status union as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        ctx: MsgCtx,
    },
}
