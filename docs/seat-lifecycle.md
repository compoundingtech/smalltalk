# Seat lifecycle

A seat's declaration, work, messages, and decisions live in the graph. A running harness also has its own native session. Restarting a process does not erase the plan; preserving the **exact native conversation** is what suspend/resume is for.

Use the worker from [getting started](getting-started.md). Check it before changing anything:

```sh
st agents show agent/garden/worker
st agents queue agent/garden/worker
```

## Restart, stop, or suspend?

| Need | Action |
| --- | --- |
| Recover a stuck harness or apply new launch settings | Restart. Keeps its declaration and waits for a new incarnation. |
| Finish using this seat | Stop. It remains stopped until explicitly started or reapplied. |
| Pause a quiet seat and return to exactly the same conversation | Suspend, then resume on the same host. |

Restart a seat:

```sh
st agents restart agent/garden/worker --as person/ada
st agents show agent/garden/worker
```

When repeated failed starts have parked a seat, a person's explicit restart permits one launch attempt for that request. Replaying the request or restarting the daemon does not grant another attempt. If the retry fails, the seat parks again and its cause stays visible in `st agents show` and the agents view. A new restart request permits a new retry. Provider version checks and native-session continuation still apply.

The graph retains claimed work and pending mail. A relaunch continues the previous native session when the harness can; otherwise it starts a new session and recovers work from the graph. Stop and start explicitly when you want it off between uses:

```sh
st agents stop agent/garden/worker --as person/ada
st agents start garden/worker --as person/ada
```

To preserve the exact conversation, wait until the seat has finished its turn and holds no claim:

```sh
st agents suspend agent/garden/worker --as person/ada --reason 'Idle until tomorrow.'
st agents show agent/garden/worker
st agents resume agent/garden/worker --as person/ada
```

Suspend refuses a busy seat with its blockers: a turn, claimed work, pending question, unsent input, or running subagent. A new harness may need one turn before it has a saved native session. Mail waits while suspended; it does not wake the seat. Resume verifies the same session on the same host. See [suspend/resume](st3/suspend.md) for failure reasons and limits.

## One-shot seats

Declare `one-shot` on a seat that should finish when its process exits:

```kdl
version 2
agent "garden/interactive" {
  workspace "/work/garden"
  harness "omp" {}
  one-shot
}
```

The daemon retires the seat after its process exits, including an unsuccessful exit or a
process it discovers has vanished. It disappears from `st agents ls`; `st agents ls --all`,
`st agents show agent/garden/interactive --all`, and subject history retain it. No launcher or
post-exit wrapper needs to stay alive. Retirement takes precedence over the declaration's
restart policy. A seat without `one-shot`, including one with `restart "never"`, keeps its
usual lifecycle.

`st agents start garden/interactive --as person/ada` restores the same declaration and native
session. Explicit restart, launch-setting changes, and suspend/resume still work while the
seat is live. A one-shot mission seat stays retired for its current generation until explicitly
started; a later generation can declare it again.

A one-shot member of an owned set retires without changing its source bundle or ownership.
Publish a new declaration through that set to start it again; ordinary `agents start` continues
to respect the set's ownership.

## Find a workspace and return to an interactive seat

Read the directory from the seat instead of constructing a path from its identity:

```sh
st agents workspace agent/garden/interactive
st agents workspace agent/garden/interactive --json
```

The plain command prints only the declared workspace directory, so a local launcher can use
`workspace=$(st agents workspace agent/garden/interactive)`. The JSON envelope also names
`host_id`: the path belongs to that host, even when read through another fleet member. It is the
workspace root, not an internal harness directory or a guarantee that the directory still exists.
The read works while the seat is running, stopped, suspended, or retired as a one-shot. It follows
an unambiguous stop back to the same declaration; unknown or conflicting declarations produce
an error instead of a guessed directory.

For a running interactive seat, attach to its existing terminal:

```sh
st terminals attach agent/garden/interactive --as person/ada
```

Attach does not start or restart the seat. Detach with Ctrl-\\; the harness keeps running, and
the same attach command reconnects. After the process exits or you stop it, restore the same
seat and attach again:

```sh
st agents stop agent/garden/interactive --as person/ada
st agents start agent/garden/interactive --as person/ada
st agents workspace agent/garden/interactive --json
st terminals attach agent/garden/interactive --as person/ada
```

Omit `stop` if the seat has already stopped or retired. Start without overrides restores its
original harness, host, workspace, and saved native session. Native continuation depends on the
harness having saved its session; use suspend/resume when you need st to verify the exact native
session before resuming. A mission's `fresh-context` policy intentionally replaces native context
before claimed work; see [mission runtime](st3/mission-graph-runtime.md). An owned-set seat must
be restarted through its source publication.

Typed harness declarations start without a boot task prompt and wait for input unless an initial
message is supplied. Launchers need no separate no-boot mode and no path into st's internal state.
The equivalent client-v0 read is documented under
[seat workspace directories](st3/client-v0/README.md#seat-workspace-directories).

## Bring an existing native session under st

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

## Spot and recover a stale or stopped seat

```sh
st agents ls --all
st agents show agent/garden/worker --json
st subject history agent/garden/worker --limit 20
st doctor
```

Read the selected declaration, latest runtime, and stop/suspension reason together. A seat hidden from the default list can still be shown by its exact ID. A `stale` delivery path means its message channel is not proving readiness; `outdated` means it is running older code. Check the local host's doctor report and any login/trust prompt before restarting. Resume a suspended seat; start an intentionally stopped one; restart a stale running one after fixing the cause.

Daemon restarts adopt running seats. Current drivers and channels follow a replaced binary without restarting the provider session; [seats across deploys](st3/seat-deploys.md) explains their `current`, `outdated`, `legacy`, and `stale` reports. See [troubleshooting](when-something-is-wrong.md) when a seat stops again.

## Create a seat from the CLI

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

## Declare and update a seat

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

See [model accounts](st3/accounts.md) for usage totals, weekly limits, and account pools.

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
systemd user manager; [Stopping a session](st3/priority.md#stopping-a-session) says what
other hosts miss. A mission seat you stop stays stopped while its run's generation lasts, across
daemon restarts, and `st agents start` starts it again on its run's own declaration, creating no
other seat.

`st agents suspend agent/example/worker --as person/ada` stops a quiet seat and keeps its harness's
own session; `st agents resume agent/example/worker --as person/ada` brings the seat back on that
same session and checks it. A seat suspends only when it is idle, with no pending question,
claimed step, or running subagent. A suspended seat stays declared, is not restarted, and keeps
its mail until it resumes. [Suspending a seat](st3/suspend.md) has the details.

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

[`examples/st3/seats`](../examples/st3/seats) has a seat file for each harness.
