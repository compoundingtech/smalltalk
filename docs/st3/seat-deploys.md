# Seats across deploys

A deploy installs a new st binary at the same path and then restarts the daemon. The daemon adopts
every running seat; it does not restart them. This document explains how each seat's message path
follows the new binary without ending the provider session, and how st reports a seat whose message
path did not.

## What runs in a seat

Each seat runs in its own PTY. The daemon starts `st3 driver HARNESS` there, and the driver starts
the provider. Several st processes carry the seat's messages for the whole provider session:

| Harness | Driver | Channel |
| --- | --- | --- |
| Claude | polls the mailbox, writes each message to the seat's native inbox, publishes harness state | `st3 driver claude-mcp`, which Claude starts, hands inbox files to the TUI |
| Codex | polls the mailbox and submits each message over its control connection to the app-server that the TUI also uses | none |
| OpenCode | polls the mailbox and posts each message to the loopback server in the OpenCode TUI | none |
| pi, omp | keeps presence and the terminal record | `st3 driver pi-channel` or `omp-channel`, which the extension starts, polls the mailbox and hands each message to the provider |

A long-lived process keeps executing the file it started from. Linux names that image
`PATH (deleted)` once the file is replaced. Before this change the processes above kept talking to
the new daemon with the old code, and the old driver's control loop ended on a response it did not
understand while its provider kept running. Messages then stayed `sent`, and only a harness restart
brought delivery back.

## Following a replaced binary

The driver and both channels watch the installed binary: `ST3_BIN`, which the daemon sets to its own
executable path, or else the path the process started from. Once a second the process compares that
file's device, inode, size, and modification time with its own running image. A different file is a
replacement. The process acts on it only after the file stays unchanged for a second and answers
`st3 resume-probe` with the resume format the process writes. A replacement that cannot read that
format, such as a rollback to a build from before this change, is refused, and the process keeps its
current code and tries again 30 seconds later.

To follow the replacement, the process writes its live state to a private file, blocks the stop
signals, and calls `execve` on the new binary with its own arguments and the state file's path in
`ST3_DRIVER_RESUME` or `ST3_CHANNEL_RESUME`. `execve` keeps the PID, the parent and child
relationships, the terminal, and every descriptor without close-on-exec. The new image reads the
state back, installs its stop handlers, and then unblocks the signals. A stop that arrived in between
was pending the whole time, so it ends the session the ordinary way. If `execve` fails, the process
adopts its own session again and keeps running.

A driver releases its provider before it executes. The provider loop sees `DETACH`, returns the
session instead of ending it, and writes no terminal record. The next image adopts it:

- Claude, pi, and omp: the provider PID and the observed session's token and sequence. The new image
  keeps presence, heartbeats, the stop path, and the terminal record exactly as the launching image
  did.
- OpenCode: the provider PID, the server's port and password, and the observed session. The new
  image reconnects to the same server, reseeds observed state from it, and continues delivery from
  the durable ledger.
- Codex: the TUI, the app-server, its watchdog, and the watchdog pipe's write end. The watchdog kills
  the app-server's process group when that pipe closes, so the pipe is the one descriptor the driver
  keeps across the exec. The new image opens a new control connection to the same app-server,
  resumes the bound thread, reconciles delivery from the thread and the ledger, and supervises the
  running TUI. A Codex driver follows a replacement only once its thread is bound.

The driver also carries its published timeline, its ready flag, and its delivery episode number, so
the new image republishes nothing. The Claude channel carries the MCP handshake, the inbox files it
already handed to Claude, and any partial request line. The pi-family channel carries its delivered
and failed messages, its unsent reports, and any partial frame; it skips the hello, because the
extension already has its session context. Both channels read stdin on a thread that polls with a
short timeout and is joined before the exec, so no byte they took from the pipe is lost.

A driver's control loop now ends only when its provider ends. A failed publish is logged and retried,
and delivery runs before every other publish on each tick, so no observation can hold a message back.

## Reporting a stale path

Every mailbox poll from a seat's delivery process carries a small report: the transport, the
process's PID, its running image, and for Claude the channel's PID, image, and the age of its last
presence write. The daemon keeps the latest report for each recipient in memory. It is not graph
state; a restarted daemon learns every live path again within a second.

`st agents ls` and `st agents show` add a `delivery` object to each local native seat whose harness
can take work:

- `current`: a poll arrived in the last 45 seconds from a process running the daemon's own image,
  and for Claude the channel reported in the last ten seconds from that image too;
- `unknown`: the daemon started less than 20 seconds ago and the seat has not polled yet;
- `stale`, with the reason: no poll since then, a poll from a replaced or unidentified binary, or a
  silent or replaced Claude channel.

A stale seat shows as `waiting` instead of `running`, and `st agents show` prints the reason on its
`DELIVERY` line. Remote seats carry no `delivery` object; only their own daemon can see their polls.

## Deploys

A deploy only needs to install the binary and restart the daemon. Each seat follows the replacement
within about two seconds of the install. The first deploy of this change is the exception: seats
started before it run drivers that cannot follow a replacement and do not identify themselves, so
they show as `waiting` with a delivery path that predates delivery reports. Restart each of them once.

The path must stay the same. A deploy that starts the daemon from a new path, such as a new store
path, leaves every running seat on the old one, and st reports them as stale.

`scripts/st3-graceful-messaging-eval/run` proves this end to end for each harness; see
[the eval](../../evals/st3/graceful-messaging/README.md).
