# Small Talk

## Continuous integration

Small Talk runs pull request and main branch CI on our own Linux and macOS machines through
`st`. The `st/ci` commit status reports the Linux debug workspace tests and Clippy;
`st/ci-macos` reports the same checks on macOS. GitHub Actions handles tags and forked
pull requests. See [CI operations](docs/ci.md) to inspect a failing run.

Small Talk (`st`) runs coding agents as durable seats and hands them work as missions. The graph
records every seat, mission, step, message, and decision, so the state of your agents survives
harness, daemon, and machine restarts. One daemon runs on each machine; machines can join a fleet.

## Install

Download prebuilt Linux x86_64 or macOS arm64 tools from [tagged releases](docs/st3/binary-releases.md),
or build from source below.

With Nix, from a checkout:

```sh
nix profile install .#st3
```

For local development, enter `nix develop`. The shell provides Rust, sccache,
cargo-nextest, and mold on Linux. Cargo uses mold for Linux links and the system
linker on macOS; dev and test builds keep line tables for workspace crates and
omit dependency debug info. On both platforms the shell sets `RUSTC_WRAPPER` to
sccache. Run tests with `cargo nextest run --workspace --locked`.
Outside the Nix shell, install mold on Linux and cargo-nextest separately; the
repository's `.cargo/config.toml` still selects mold for Linux builds.

This installs `st3`, the `st` symlink, the `stui` terminal app, `st3-migrate`, and the pinned
`pty` terminal runtime.

Without Nix, build and install from a checkout with a Rust toolchain:

```sh
scripts/install                  # into ~/.local/bin
scripts/install --bin-dir DIR    # or anywhere else
```

The script builds everything once, installs `st3`, `stui`, and `st3-migrate` (and `st2`, the
previous generation), and makes `st` a symlink to the installed `st3`. `st` is never a separate
build. A source install also needs [`pty`](https://github.com/compoundingtech/pty-rust) on `PATH`.

Each seat runs a coding harness, so install and log in to at least one: Claude Code, Codex, omp,
pi, or OpenCode. Log in as the same user that runs the daemon.

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

st records every `git` and `gh` call it starts, including its own, in
`~/.local/state/st3/recorder/commands.jsonl`, then runs the real program unchanged. A call by
absolute path is not recorded. The [command recorder](docs/st3/command-recorder.md) describes the
log.

If the state directory has a long path, set `XDG_RUNTIME_DIR` to a shorter directory or pass
`--socket` and `--client-gateway-socket` to `st up` so both Unix socket paths fit the OS limit.

## First commands

```sh
st now                  # what needs you right now
st agents ls            # seats and other running agents
st missions ls          # missions with current runs
st work ls              # steps that are ready or in progress
st attention ls         # decisions and requests waiting for you
st conversations ls person/ada
```

Lists show current state. Add `--all` for history. Every command has `--help`, and the global
`--json` flag prints the stable client format that the apps read. Run `stui` for the same views
in a terminal app.

## Declare a seat

A seat is a durable agent: a harness, a model, and a workspace, kept running by st. Write its
declaration with `st agents start --print-kdl` and keep the file in Git:

```sh
cd ~/src/garden
st agents start example/worker --harness claude --model claude-sonnet-5 --effort medium \
  --as person/ada --print-kdl > worker.kdl
```

```kdl
version 2
agent "example/worker" {
    workspace "/home/ada/src/garden"
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

The seat starts its harness in the workspace. Its first turn reads the generated
`.st3/boot.md`, checks for work, and waits. `st terminals attach agent/example/worker` opens its
terminal; Ctrl+\\ detaches without stopping it.

A running seat keeps its current process when you apply a changed declaration; the change takes
effect the next time it starts. To use it now, stop the seat and apply again. `st agents stop
agent/example/worker --as person/ada` stops a seat until you apply its file again.

[`examples/st3/seats`](examples/st3/seats) has a seat file for each harness.

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
seat's queue, st wakes the seat, and the seat claims the step, does it, and completes it.
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
```

`st conversations sessions` lists harness sessions and `st conversations timeline SESSION`
shows one session's messages and tool calls.

## Bring in sessions st does not own

st can find Codex, Claude Code, omp, pi, and OpenCode sessions you started yourself and move one
under its ownership:

```sh
st import ls            # running sessions
st import ls --all      # saved sessions too
st import show SESSION
st conversations timeline SESSION
```

`import show` gives the harness, the native session ID, the workspace, and the process, if one is
running. A running harness whose command line does not name its session is listed as blocked; exit
it, and import its saved session from `import ls --all`. Check the workspace before you import:
do not import a session that a seat is already running.

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
