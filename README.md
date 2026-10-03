# Small Talk

## Continuous integration

Small Talk runs pull request CI and every push to `main` on GitHub Actions with Namespace runners.
Main pushes each run Workspace CI and macOS CI independently. `linux-gate`, `isolation-vm` and
`genie-freshness` are the required checks; `macos-ci` is optional on pull requests and runs
when a pull request carries the `macos-ci` label. A ready pull request lands through GitHub's
merge queue: `gh pr merge NUMBER --auto`. See [CI operations](docs/ci.md) for the queue and to
inspect a failing run.

Small Talk (`st`) runs coding agents as durable seats and hands them work as missions. The graph
records every seat, mission, step, message, and decision, so the state of your agents survives
harness, daemon, and machine restarts. One daemon runs on each machine; machines can join a fleet.

### Shared UI models

`crates/st3-ui-model` provides renderer-independent UI semantics without a ratatui dependency.
Its broad name is intentional; initially it contains only stui's mission model (`Word`,
`StepState`, `Mission`, `Step`) and mission derivation. stui consumes that same model.
`missions::adapt` borrows typed mission and agent projections plus unresolved, actor-filtered
attention. Callers supply the current time and display policies explicitly; collection loading,
clocks and application naming remain outside the crate. Mission precedence, queue/keep-open
rules, outcomes and rich step details retain stui's existing behavior. Shared widgets and other
UI models are separate follow-up work, not part of this mission extraction.

## Install

Download prebuilt Linux x86_64 or macOS arm64 tools from [tagged releases](docs/st3/binary-releases.md),
or build from source below.

With Nix, from a checkout:

```sh
nix profile install .
```

For local development, enter `nix develop`. The shell provides Rust, sccache,
cargo-nextest, and mold on Linux. Cargo uses mold for Linux links and the system
linker on macOS; dev and test builds keep line tables for workspace crates and
omit dependency debug info. On both platforms the shell sets `RUSTC_WRAPPER` to
sccache. Run tests with `cargo nextest run --workspace --locked`.
Outside the Nix shell, install mold on Linux and cargo-nextest separately; the
repository's `.cargo/config.toml` still selects mold for Linux builds.

This installs `st3`, the `st` symlink, the `stui` terminal app, `st3-migrate`, and the pinned
`pty` terminal runtime. The default, `st`, `st3`, and `small-talk` Nix package names all select
this package. The previous generation is built and tested separately as `.#st2`;
install it explicitly with `nix profile install .#st2`.

Client terminal screens use `pty-terminal` and the same pinned `libghostty-vt` artifact as
the PTY runtime. Styled runs carry terminal-cell widths (including wide characters), soft-wrap
continuations, strikethrough and admitted OSC 8 links; keyboard modes include kitty flags.
The Nix package and developer shell link the shared static library through pkg-config, so
building Small Talk does not require Zig or a Ghostty source checkout.
The runtime, screen projector and terminal UI pin their PTY protocol crates to the same
producer revision, keeping one shared protocol source in the workspace.

Without Nix, build and install from a checkout with a Rust toolchain and the matching
`libghostty-vt` artifact. Set `PKG_CONFIG_PATH` to its `share/pkgconfig` directory;
`pkg-config --static --libs libghostty-vt-static` must resolve before building:

```sh
scripts/install                  # into ~/.local/bin
scripts/install --bin-dir DIR    # or anywhere else
```

When pkg-config cannot find the artifact, Cargo builds `libghostty-vt` from the pinned Ghostty
source instead, which needs Zig 0.15.2 on `PATH`. On macOS, the vendored
[`libghostty-vt-sys`](vendor/libghostty-vt-sys/README.md) build script lets Zig 0.15.2 link against
the macOS 26.5 and 27 SDKs.

The script builds and installs `st3`, `stui`, and `st3-migrate`, and makes `st` a symlink to
the installed `st3`. `st` is never a separate build. On macOS, both tools live in a fixed app bundle; see [macOS installation and signing](docs/st3/macos-installation.md). A source install also needs [`pty`](https://github.com/compoundingtech/pty-rust) on `PATH`.

Each seat runs a coding harness, so install and log in to at least one: Claude Code, Codex, omp,
pi, or OpenCode. Log in as the same user that runs the daemon.

### Home Manager

Import `inputs.smalltalk.homeManagerModules.default` and configure the user service:

```nix
services.smalltalk = {
  enable = true;
  person = "person/ada";
  declarations = {
    seats = [ ./seat.kdl ];
    missions = [ ./mission.kdl ];
  };
  # ptyPackage = inputs.pty.packages.${pkgs.system}.default;
};
```

The module installs `st3`, writes `st3/config.toml`, and starts a systemd user
service on Linux or a launchd agent on macOS. When declarations are provided,
an additional oneshot service applies seat files and publishes mission files
as `person`, retrying while the daemon starts. `declarationsApply.enable = false`
disables this step. Reapplying is safe, but removing a file does **not** delete
its previously published declaration until managed-set apply exists (#646).
The optional `ptyPackage` replaces the bundled `pty` for both the daemon and
seats, working around the executable-directory PATH precedence in #633.

Activation atomically installs a real `st3` executable at `stateDir/bin/st3` (by default
`~/.local/state/st3/bin/st3`) before restarting the daemon, with `st` as an alias in that directory.
The daemon, declaration-apply service, and service PATH use this stable location so running seats
can follow new builds without ending their harness sessions. Unchanged contents are not rewritten.
Package references in the systemd unit or launchd plist still trigger daemon restarts on upgrades.
Seats started from store paths before this change remain stale until restarted; see
[seats across deploys](docs/st3/seat-deploys.md).

Linux user-manager lingering and macOS `st service permissions` remain host
setup prerequisites. Fleet/replication setup is not managed by this module.

## Run the daemon

Tell st who you are. Commands that act for a person read this, so you do not repeat `--as` on
every command. The file is `$XDG_CONFIG_HOME/st3/config.toml` when that variable is set:

```sh
mkdir -p ~/.config/st3
printf 'person = "person/ada"\n' > ~/.config/st3/config.toml
```

Then install the daemon as a user service and check it:

```sh
st service install
st service status
st doctor --strict
```

Run `st service install` again after upgrading the binaries. It updates the installed definitions
and restarts the services so they use the new executables.

On Linux the service is a systemd user unit. It needs a working user manager; enable lingering
(`loginctl enable-linger`) so seats keep running after you log out. On macOS it is a launchd
agent; run `st service permissions` once for the Full Disk Access and Developer Tools steps.

State lives in `~/.local/state/st3` and the local API is a Unix socket. Restarting the daemon
does not stop running seats; it adopts them. While it restarts, a command waits up to 30 seconds
for it (`--daemon-wait SECONDS` or `ST3_DAEMON_WAIT` changes that) and then exits with status 5.
Seat drivers wait as long as the restart takes, keep their notes out of the seat's terminal in
`~/.local/state/st3/driver-api-warnings.log`, and resume from the graph. To run the daemon in the
foreground instead, use `st up`.

On Linux, the daemon normally listens in `XDG_RUNTIME_DIR`; it also publishes
`STATE/run/st3.sock` as a link to that socket, so commands without the daemon's
runtime environment can reach it. On macOS the socket already lives at that state path.
An explicit `--endpoint` or `ST3_ENDPOINT` still takes precedence.

st records every `git` and `gh` call it starts, including its own, in
`~/.local/state/st3/recorder/commands.jsonl`, then runs the real program unchanged. A call by
absolute path is not recorded. The [command recorder](docs/st3/command-recorder.md) describes the
log.

If the state directory has a long path, set `XDG_RUNTIME_DIR` to a shorter directory or pass
`--socket` and `--client-gateway-socket` to `st up` so both Unix socket paths fit the OS limit.
When `st up` receives a private `--state-dir` or `--socket` without an explicit
`--client-gateway-socket`, its paired gateway is placed beside that private socket (or
in the private state directory when no socket is specified). An existing live listener
at either socket path is never replaced; choose a different path instead.

## Run st on more than one machine

Install st on each machine. On an existing listening member, run `st fleet invite beacon`.
On the new machine, run `st fleet join` and paste the code when asked, then check
`st fleet status` and `st replication status`. These steps are the same for a laptop or server.
Join installs the services and catches up the replica over Tailscale or Fabric.

Any member may be offline. It shows its last exchange time, retries with increasing delays,
and announces itself when it returns so one successful connection synchronizes both directions.
A machine that can only connect out needs no special sync setting.

See [fleet replication](docs/st3/replication.md#add-any-machine) for explicit tailnet/Fabric
routes and Fabric inbox invitations, and [fleet join](docs/fleet-join.md) for transport trust,
removal, and migrating an existing fleet off local dial helpers. A client-only stui or app
instead pairs as a [device](docs/st3/client-only.md), keeping a cache rather than a full replica.

The [sync invariants](docs/st3/replication.md#sync-invariants) require canonical shared ordering
and digest coverage for every synced logical source and shared projection.

## First commands

```sh
st now                  # what needs you right now
st agents ls            # seats and other running agents
st missions ls          # missions with current runs
st work ls              # steps that are ready or in progress
st attention ls         # decisions and requests waiting for you
st conversations ls person/ada
```

`st attention approve ID --as person/ada` answers a gate waiting for you by the ID
`st attention ls` prints; `reject` and `request-changes` also take `--reason TEXT`. A gate
that no longer waits says why: who answered it, or what changed since it asked.

`st --help` and `st help` open with the main uses, then group commands for everyday use, agent
seats, and running a machine or fleet. `st help --all` also lists plumbing commands.
Use `st help agents new` to open a command's full help.

Lists show current state. Add `--all` for history. Every command has `--help`, and the global
`--json` flag prints the stable client format that the apps read. Run `stui` for the same views
in a terminal app.

### Embedding native conversations

`crates/st3-conversation-ui` provides the same native conversation presentation used by
`stui`, without terminal acquisition, application tabs or daemon connections. Feed
`st3-client` conversation frames into `Timeline::apply(Frame { replace, has_more, items })`,
adapt the timeline with caller-owned display names, and render it with `Cache::render`
and caller-supplied `Theme` tokens. The returned document contains styled lines and typed
`PaneIntent` targets. `State` retains scrolling, tool expansion, display-column selection
and composer drafts; send, open and older-history intents are executed by the embedding app.
Replacement frames remove the previous bounded window; incremental frames revise entries
by ID. `has_more` remains available so a client can distinguish bounded from complete history.

The shared crate and `stui` use workspace Ratatui 0.29. An embedding application using
Ratatui 0.30 must align its rendering dependency before passing buffers or lines across this
boundary. Native conversations are not a terminal emulator: harness menus and arbitrary
permission prompts still require access to the harness terminal.

## Start an agent

One command declares a seat on any fleet machine, waits until its harness is ready, and attaches
this terminal to it:

```sh
st agents new site --host builder --harness claude --model claude-opus-5-5 --attach
```

It writes the same declaration a person writes by hand, with the harness defaults of the fleet's
Claude and Codex seats, and applies it as the `person` in your st config (or `--as`). Without
`--workspace`, the agent gets a new directory below that host's home, `~/st/agents/site`, which the
host creates. Its next-step commands name the agent's subject, here `agent/builder.site`. `--print-kdl` shows
the declaration without applying it, and `--description` says what the agent is for.
`--message "Inspect the failing tests"` supplies an explicit first message through the harness's
native startup argument. With no message, the seat starts idle. The Rust `st3-client` crate exposes
`agent_create` with the same creation options; TypeScript and Swift expose `agentCreate`.

Ctrl+\\ detaches and leaves the agent running. `st terminals attach agent/builder.site` attaches
again later, from any machine in the fleet. A terminal on another host is attached PTY to PTY over
Fabric when that host runs `st terminals expose-fabric` and grants your machine the protocol it
prints; no st daemon carries the bytes. Otherwise it goes through the client gateway as your
person, the same path the apps use.

After creating a seat, st explains whether it is ready or still starting and prints the exact
commands to attach, send it a message, inspect it, and stop it. These commands are also printed
when startup times out or the agent needs your input. Attaching stays opt-in with `--attach`.
Mission starts, launches, device pairing, and fleet creation or joining also finish with their
next steps. JSON output keeps its existing shape.

## Open a shell

```sh
st terminals new work --cwd /tmp
st terminals attach terminal/pty/person/ada/UUID
st terminals end terminal/pty/person/ada/UUID
```

`terminals new` prints the new terminal ID. It runs the host's preferred shell (`$SHELL`, falling
back to `/bin/sh`) with no agent harness. Locally its directory defaults to your current directory;
with a remote `--host`, pass an absolute `--cwd` or use that daemon's directory. The shell does not
restart after exit. `end` publishes a durable stop. Only its creator may create or end its declaration.
The client crates expose `terminal_create` and `terminal_end` (camel case in TypeScript and Swift).

## Declare a seat

A seat is a durable agent: a harness, a model, and a workspace, kept running by st. Write its
declaration with `st agents start --print-kdl` and keep the file in Git:

```sh
cd ~/src/garden
st agents start example/worker --harness claude --model claude-sonnet-5 --effort medium \
  --as person/ada --print-kdl > worker.kdl
```

`agents start` accepts an identity or a complete agent subject: `example/worker` and
`agent/example/worker` both declare `agent/example/worker`. A slash-qualified or dotted identity
is exact; a simple name such as `worker` becomes `agent/HOST.worker` on its placement host.
Use `agent/HOST.worker` to name that seat explicitly. A doubled `agent/agent/` prefix is rejected.

Starting an existing seat preserves its declaration, including its restart policy and command.
Only explicit `--host`, `--workspace`, `--harness`, `--model`, `--effort`, and `--arg` options patch
it. `--model`, `--effort`, and `--arg` require an existing typed harness and are refused for
`command`/`argv` seats. For a new seat, the defaults remain Claude, the current workspace, and
`restart always`. `--print-kdl` queries the daemon to preview the effective declaration without
publishing it.

```kdl
version 2
agent "example/worker" {
    workspace "/home/example/src/garden"
    restart always
    harness claude {
        model claude-sonnet-5
        effort medium
    }
}
```

Apply it and look at it:

```sh
st agents apply worker.kdl --as person/ada
st agents show agent/example/worker
st terminals peek agent/example/worker
```

Token spend across the fleet is available by agent, mission, step, model, account, or host, with
its API-equivalent cost: what the tokens would cost at the provider's list price, from a pricing
table built into st. Tokens on a model the table does not price are counted as unpriced, and a
cost that leaves them out ends in `+`. An account is a short digest of the harness's own login,
never the login itself. The graph keeps hourly usage for about a week and each series' total
after that; `[observations.otlp]` sends every response to OpenTelemetry for longer history. The
period ends now:

```sh
st usage --hours 24
st usage --hours 24 --by step
st usage --hours 24 --by account
```

`st usage` also lists each account's 5-hour and weekly limits: the freshest reading any seat on
that account reported, with when it was measured and when the weekly window resets.

A node can stop an account's seats as the account nears its weekly limit. It is off until its
config enables it:

```toml
person = "person/ada"

[limits]
enabled = true
stop_at_weekly_percent = 95
keep = ["agent/example/coordinator"]
fresh = "1h"
```

When an account's freshest weekly reading, no older than `fresh`, reaches the percentage, each
node stops the seats it hosts on that account, except those in `keep`, and the node that measured
the reading puts one request on the person's home naming the stopped seats and the reset time.
Each seat is stopped once per weekly window: start it again and it stays up until the reset. Give
every node that hosts seats the same `[limits]`.

The seat starts its harness in the workspace with no prompt. It stays idle, taking no turn, until
a person types in its terminal or a message is posted to it. A seat you declare as
`fleet/PROJECT/...` may also publish, start, and revise missions under `fleet/PROJECT/*`;
`st agents show` prints that authority. `st terminals attach agent/example/worker` opens its
terminal from any fleet machine; Ctrl+\\ detaches without stopping it. On the seat's own host it
connects straight to the PTY session, so a busy daemon cannot stall it. If the daemon does not
answer within a second, st attaches to the seat's newest PTY session on that host without it and
says so.

A running seat restarts when you apply a declaration that changes how it launches: its
workspace, harness, model, effort, arguments or command. A change to its label, environment or
restart policy keeps the running process and takes effect the next time it starts; use `st agents
restart agent/example/worker --as person/ada` to apply it now. Restart preserves the declaration,
works for top-level and mission seats, and waits for a new running incarnation.

Every relaunch of a seat continues its harness's last native session: `st agents restart`, a
changed launch, a harness that hung up or crashed, a daemon restart, and a stop followed by a
start. Claude, Codex, pi, omp and OpenCode each resume the session their driver last reported for
the seat, and a Claude seat whose workspace changed carries its transcript into the new
workspace's project. A harness that cannot continue that session (its transcript is gone, or the
declaration selects its own session) starts a new one and records a
`native-continue-unavailable` warning; later relaunches do not try that session again. Only a
mission step with `fresh-context` starts a seat on a new session on purpose. `--timeout 2m` changes the default ten-minute
wait; a failure or timeout explains why the seat is not running again. `st agents stop
agent/example/worker --as person/ada` stops a seat until you apply its file again. It ends every
process the seat started, including builds and tests that outlived the harness, on hosts with a
systemd user manager; [Stopping a session](docs/st3/priority.md#stopping-a-session) says what
other hosts miss. A mission seat you stop stays stopped while its run's generation lasts, across
daemon restarts, and `st agents start` starts it again on its run's own declaration, creating no
other seat.

`st agents suspend agent/example/worker --as person/ada` stops a quiet seat and keeps its harness's
own session; `st agents resume agent/example/worker --as person/ada` brings the seat back on that
same session and checks it. A seat suspends only when it is idle, with no pending question,
claimed step, or running subagent. A suspended seat stays declared, is not restarted, and keeps
its mail until it resumes. [Suspending a seat](docs/st3/suspend.md) has the details.

Human labels are presentation, not launch configuration:

```sh
st agents rename agent/example/worker "Garden maintenance" --as person/ada
st agents rename agent/example/worker --clear --as person/ada
```

Rename publishes only `desired.display_name`, the same durable field as KDL `name`. It does not
restart the seat or change its identity, and a stopped seat keeps its restart budget and any
crash-loop hold. The Agent API uses this effective label; clearing it restores `example/worker`.
The label must be non-empty. Like declaring a seat in free mode, a person or an agent may rename
any seat. A bound harness must act as itself, and rename preserves the original declaring actor.

[`examples/st3/seats`](examples/st3/seats) has a seat file for each harness.

## Observe, don't instruct

st records what an agent does; it does not tell the agent how to behave. Each harness driver
reports what it can see: sessions, turns, plan mode, subagents, tool calls, token usage, and what
the seat is blocked on. st never asks an agent to report on itself. A seat starts idle unless its creator explicitly supplies `--message`. Besides the messages and step goals that carry work, st's only text for agents is the
skill that `st skill` prints, which describes how to use st. Each harness driver installs that
skill when its seat starts; `st skill install` writes the same files directly.

## Give it work

Work reaches a seat as mission steps assigned to it. A mission is a KDL file:

```kdl
version 2

mission "example/first-note" state="ready" {
  goal "Leave a short note in the workspace."

  step "write-note" timeout="10m" {
    assigned-to "agent/example/worker"
    goal "Create notes/hello.md in ${ST_WORKSPACE} with one sentence that says what this project is for."
  }
}
```

Publish it once, then start a run:

```sh
st missions publish first-note.kdl --as person/ada
st missions start example/first-note --id example/first-note/1 \
  --workspace ~/src/garden --as person/ada --follow
st missions show mission-run/example/first-note/1
```

Publishing stores an immutable definition. Each `missions start` is one run. The run joins the
seat's queue. When the step is ready, st posts the seat a message that names it, and the seat
claims the step, does it, and completes it.
`--follow` returns when the run ends. While it runs:

```sh
st work ls
st agents queue agent/example/worker
```

A seat holds one step at a time and takes the next ready step in queue order. Steps can depend on
each other, run in parallel, wait on gates, loop until a check passes, or ask a person to decide;
see [the examples](examples/st3/README.md).

## Talk to a seat

Messages are for conversation, not for handing out work:

```sh
st conversations send agent/example/worker --from person/ada \
  --subject "Hello" --body "Reply with one short sentence."
st conversations ls person/ada
st conversations read MESSAGE --as person/ada
st conversations thread MESSAGE
st conversations archive MESSAGE --as person/ada
st conversations search "release date" --agent agent/example/worker --since 2026-10-01T00:00:00Z
```

If a send or reply goes unanswered, st retries once with the same message and idempotency
key. If delivery remains unconfirmed, the error prints the key: rerun the same command with
`--idempotency-key KEY` to recover its result without sending a second message. Use a new key
for a new message.

`st conversations sessions` lists harness sessions and `st conversations timeline SESSION`
shows one conversation as stui shows it: messages, Small Talk, and tool calls folded to a line
or two. `--raw` prints every stored entry instead (message boundaries, tool input and output in
full), and `--json` prints the page.

`conversations search` searches the authenticated person's sent and received messages and
the normalized transcripts they can view. It returns conversation and entry IDs with short
excerpts, newest first. Use `--cursor` for older matches, or `--json` for the typed client
response. The response dates its index and reports incomplete sources, including retained
history limits and unavailable hosts. See [conversation search](docs/st3/conversation-search.md)
for freshness, costs, and the embedding API.

## Bring in sessions st does not own

st can find Codex, Claude Code, omp, pi, and OpenCode sessions you started yourself and move one
under its ownership:

```sh
st import ls            # running sessions
st import ls --all      # saved sessions too
st import show SESSION
st conversations timeline SESSION
```

`import ls` reads exact transcript paths named by live harness commands without scanning saved
history; `import ls --all` lists saved transcripts as well. The daemon reads saved transcripts in
the background, starting when it starts; `import ls --all` and `import show` answer from the
latest complete read within two seconds. Right after a daemon start on slow storage, they may ask
you to retry. `import show` gives the harness, native session ID, workspace, and process, if one is
running. A running harness whose command line does not name its exact transcript is listed as
blocked; exit it, then import its saved session from `import ls --all`. Check the workspace before
importing: do not import a session already run by a seat.

For omp, the inventory reads only session transcripts, not JSONL tool logs within a session's
attachment directory. If several transcript copies have the same native ID, st prefers a copy
whose workspace matches the live process and then the newest copy; import refuses copies still
indistinguishable by those rules before stopping any process.

```sh
st import run SESSION --as person/ada
```

Import stops that exact process if it is still running, declares a durable seat named
`agent/import/HARNESS/ID`, where `ID` is the one in `session/external-ID`, and starts the harness
resuming the same native session. The seat has no mission owner, so st keeps it running like any
seat you declare, and it appears in `agents`, `terminals`, and `conversations`.
`st subject show agent/import/HARNESS/ID` shows the declaration import wrote.

The resumed session keeps the settings it was saved with, such as its permission mode. To choose
them, declare the seat yourself with the same identity, workspace, and resume arguments, and keep
that file with your other seats. For a Claude Code session:

```sh
st agents start import/claude/ID --harness claude --workspace WORKSPACE \
  --model claude-sonnet-5 --effort medium \
  --arg=--resume --arg NATIVE_SESSION_ID --arg=--permission-mode --arg auto \
  --as person/ada --print-kdl > imported.kdl
st agents stop agent/import/claude/ID --as person/ada
st agents apply imported.kdl --as person/ada
```

One limit remains: a resumed Claude Code session does not load st's channel yet, so st cannot
wake an imported Claude seat for new mission work or messages. Give mission work to a seat you
declare yourself.

## Where to go next

Repository intake observes complete GitHub pull request and issue listings without using an agent turn. A subscription can name a review or triage mission without a revision suffix; each request starts that mission's current ready revision. The graph remembers each delivered pull request head and issue across intake and seat restarts, so the same item is not reviewed again. See [the intake example](examples/st3/github-intake.kdl) and [resource subscriptions](docs/st3/resource-subscriptions.md). Review workspaces belong on disk and are cleaned when the run finishes.

- [Examples](examples/st3/README.md), indexed by task: a seat for each harness, seat queues,
  one mission across several seats, parallel fan-out, GitHub intake, and waiting for checks, a
  time, a person, or another agent.
- [Running st with omp](docs/st3/omp.md): omp seat setup, behavior, and known limits.
- [Documentation index](docs/st3/README.md): architecture, the mission language, seat queues,
  fleet replication, and the client contract.
- [Guided CLI tour](docs/st3/cli-guided-tour.md): every command and subcommand.
- `st launch start`: describe work in plain language and review the mission a planner drafts
  before it runs.

This repository also carries st2, the previous generation; its guide is
[README.st2.md](README.st2.md).
