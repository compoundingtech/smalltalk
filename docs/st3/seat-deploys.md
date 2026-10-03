# Seats across deploys

A deploy installs a new st binary and then restarts the daemon, either from the same path or from a
new one such as a new Nix store path. The daemon adopts every running seat; it does not restart them.
This document explains how each seat's message path follows the new binary without ending the
provider session, and how st reports a seat whose message path did not.

## What runs in a seat

Each seat runs in its own PTY. The daemon starts `st3 driver HARNESS` there, and the driver starts
the provider. Several st processes carry the seat's messages for the whole provider session:

| Harness | Driver | Channel |
| --- | --- | --- |
| Claude | publishes harness state and subscribes to the seat record for PTY titles | `st driver claude-mcp`, started by the owned `st-channel@st` plugin, subscribes to the durable mailbox and proves native consumption from the bound user transcript |
| Codex | subscribes to the mailbox and submits messages over its control connection to the app-server that the TUI also uses | none |
| OpenCode | subscribes to the mailbox and posts messages to the loopback server in the OpenCode TUI | none |
| pi, omp | keeps presence, the terminal record, and live PTY titles | `st driver pi-channel` or `omp-channel` subscribes to the mailbox; the managed extension preserves first-idle gating and reports native acceptance separately from turn-context consumption |

New seats receive `ST3_MAILBOX_TRANSPORT=push`. Each delivery component connects to `/v1/mailbox`
over the local daemon Unix socket. The stream first replays the durable graph mailbox and full seat
record, then pushes changes. SQLite fences both subscriptions and receipts to the live runtime
incarnation and replacement owner; reconnecting an older channel cannot retake ownership, including
after a daemon restart. Socket loss creates no delivered or read receipt. Native ledgers retain
uncertain handoffs, and successful handoffs retry lost receipt acknowledgements with stable IDs.
No push component projects message bodies into `resources/inbox` or `resources/archive`.

`delivered` records native transport acceptance, not model consumption. A replacement runtime
reoffers delivered-but-unread messages with their original stable IDs, as well as sent and staged
messages. The same live channel keeps its handoff deduplication, and provider ledgers reconcile
uncertain handoffs across channel replacement. Read and closed messages are not reinjected.
If a provider accepted mail without durable consumption evidence, recovery favors another offer
over silently dropping it; recipients should record read evidence when they consume the message.

Epochs are allocated by the daemon, independently of wall-clock time. An initial bind has a stable
request token; a lost acknowledgement retries that same epoch, and retired tokens cannot allocate
another epoch after replacement. Reexec carries the returned epoch and token. Only an explicit
`stale-mailbox-session` ends a subscription as fenced. Store/read worker failures close its socket
and the current owner reconnects and replays after one second without creating a receipt.

Already-running seats keep the legacy delivery path when their binary follows a deploy. The new
transport starts at the next ordinary seat restart; deploys do not force providers to restart.
The compatibility paths and legacy marketplace entry remain until operations has switched the
seat declarations. New Claude declarations use `plugin:st-channel@st`, from `plugins/claude`.

Rollback to a binary from before push delivery requires restarting seats that started with `push`.
The historical resume format does not negotiate these new fence and pending-read fields; an older
image cannot safely adopt their channel state. Keep the push-capable binary installed for those
live seats until operations coordinates their restart into the rollback version. Forward upgrades
and seats that were already running on the legacy path continue to use ordinary adoption.

Seat updates carry `desired.display_name` and the member record, including the persona suffix.
They update the PTY title and pi/omp session name on reconnect, `session_start`, and `session_switch`.
Claude's status line reads the same graph authority on each render and chains the existing renderer.
Native `/rename` is temporary: the next authority update restores the declared name.

A long-lived process keeps executing the file it started from. Linux names that image
`PATH (deleted)` once the file is replaced. Before this change the processes above kept talking to
the new daemon with the old code, and the old driver's control loop ended on a response it did not
understand while its provider kept running. Messages then stayed `sent`, and only a harness restart
brought delivery back.

## Following a replaced binary

The driver and both channels watch the installed binary: `ST3_BIN`, or else the path the process
started from. The daemon sets `ST3_BIN` to `STATE_DIR/current/st3`, a symbolic link that the daemon
points at its own executable each time it starts, before it launches or adopts any seat. The file
behind that link therefore changes both when a deploy replaces the daemon's executable in place and
when the daemon restarts from a new path. Once a second the process compares that file's device,
inode, size, and modification time with its own running image. A different file is a replacement.
The process acts on it only after the file stays unchanged for a second and answers
`st3 resume-probe` with the resume format the process writes. A replacement that cannot read that
format, such as a rollback to a build from before reexec, is refused, and the process keeps its
current code and tries again 30 seconds later.

To follow the replacement, the process writes its live state to a private file, blocks the stop
signals, and calls `execve` on the new binary with its own arguments and the state file's path in
`ST3_DRIVER_RESUME` or `ST3_CHANNEL_RESUME`. `execve` keeps the PID, the parent and child
relationships, the terminal, and every descriptor without close-on-exec. The new image reads the
state back, installs its stop handlers, and then unblocks the signals. A stop that arrived in between
was pending the whole time, so it ends the session the ordinary way. A stop that a driver's handler
already caught on another thread is not pending, so the driver checks for one once the signals are
blocked. If one arrived, it does not execute, and it adopts its provider again and stops it. If
`execve` fails, the process adopts its own session again and keeps running.

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
the new image republishes nothing. The Claude channel carries the MCP handshake, stable identities
for uncertain native handoffs, and any partial request line. The pi-family channel carries its delivered
and failed messages, its unsent reports, and any partial frame; it skips the hello, because the
extension already has its session context. Both channels read stdin on a thread that polls with a
short timeout and is joined before the exec, so no byte they took from the pipe is lost.

A driver's control loop now ends only when its provider ends. A failed publish is logged and retried,
and delivery runs before every other publish on each tick, so no observation can hold a message back.

## Reporting a stale path

Each push subscription renews a small report, and legacy mailbox polls carry the same report: the
transport, the process's PID, its running image, the installed binary it follows, and for Claude the
channel's PID, image, and the age of its last presence write. The daemon keeps the latest report for
each recipient in memory. It is not graph state; a restarted daemon learns every live path again
within a second.

`st agents ls` and `st agents show` add a `delivery` object to each local native seat that is running
or waiting. Waiting on a human, login, or trust prompt does not hide its transport assessment;
delivery presence is independent of whether the harness can start another turn:

- `current`: a report arrived in the last 45 seconds from a process running the daemon's own image,
  and for Claude the channel reported in the last ten seconds from that image too. Pi-family
  channels must also have received the provider's idle proof and have no rejected handoff waiting;
- `outdated`: the same live, ready path, but the process or the Claude channel still runs a
  replaced st binary. It delivers with its old code. The reason says whether it is switching, because
  the file it follows now holds the daemon's image, or whether it follows another file and needs a
  seat restart to run current code;
- `legacy`: a recent metadata-free mailbox poll came from this seat's native delivery process.
  The daemon identifies the Unix peer PID, native driver command and inherited seat identity;
  ordinary mailbox reads do not count. This proves polling, while binary version and readiness
  remain unverified;
- `unknown`: the daemon started less than 20 seconds ago and the seat has not reported yet;
- `stale`, with the reason: no report since then, a report from an unidentified binary, a pi-family
  channel without idle proof or with a rejected handoff, or a silent Claude channel.

A stale seat shows as `waiting` instead of `running`, and `st agents show` prints the reason on its
`DELIVERY` line. Remote seats carry no `delivery` object; only their own daemon can see their reports.

## Deploys

Codex's `systemError` is a failed thread status, not proof that its app-server process crashed.
The driver retains the typed turn error, including when it has to recover it from the session
rollout because the control subscriber missed a completion. Policy refusals, model capacity,
credential rejection and usage limits have distinct causes. They appear in the agent's `fault`
field and the fault list; an unknown cause stays explicit rather than becoming healthy state.
Only a positive harness recovery or a replacement incarnation clears that fault; later work
progress cannot erase it. A model-capacity refusal uses the existing bounded retry in the same
session. A cyber-policy refusal needs review of the task and provider refusal before resuming;
st does not automatically retry it.

A deploy only needs to install the binary and restart the daemon. Drivers with reexec support
follow the replacement within about two seconds of the install. Older native paths can keep
delivering and report `legacy`; an older executable is not represented as a current one. A deploy
does not require restarting their provider sessions.

Pi and omp extensions reopen an unexpectedly exited channel with bounded backoff. Each new
handshake samples the provider's idle proof, even when no turn runs. Negative handoff receipts
retry indefinitely with a delay capped at five seconds; the third failure records a diagnostic
without stopping retries. The channel reports the rejected handoff as stale until native
acceptance or an authoritative delivery/read/close receipt settles it. Within the same incarnation,
this never authorizes repeating a handoff already accepted by the native transport.

Rejected-attempt counts survive channel reexec in the same incarnation and appear in its presence
and diagnostics. They are not durable `message.attempt` claims and reset on a new seat incarnation.
Recording failure history and per-attempt tokens in the shared graph is a follow-up to the fenced
receipt transport; the current lifecycle idempotency keys identify acknowledgements, not attempts.

`st conversations status MESSAGE` shows delivery/read progress without changing its lifecycle.
The client message view carries the same `delivery` assessment, including the local recipient
path when one is known. A remote path stays unverified. A message waiting more than ten seconds
for read is visible as `waiting`; it stays durable. `st doctor` warns about stale local paths and
overdue graph read receipts for current agents. It excludes obsolete seat recipients and people,
and distinguishes accepted native handoffs. A missing graph read receipt on a legacy channel
does not prove that its provider failed to consume the envelope. Reading the message clears its
pending-read warning. Both `driver claude` and the older `driver claude-mcp` are recognized as
native delivery peers; ordinary mailbox inspection never refreshes this health assessment.

Older Claude, Codex and OpenCode outer drivers poll with `include_closed=true` to archive
consumed inbox files. Those polls count as legacy delivery activity when the local Unix peer
is proven to be that recipient's native outer driver. An ordinary CLI history read, a poll for
another recipient, or a channel process's history query does not count. Claude's MCP child
watches inbox files; the outer `driver claude` process supplies the mailbox poll.

A daemon started from a new path, such as a new Nix store path, moves its seats through the
`STATE_DIR/current/st3` link. Seats launched before that link existed have `ST3_BIN` set to the
executable path they started from. A Nix store path never changes, so those seats keep delivering
with their old code and report `outdated`, naming the path they follow, until they are restarted.

The Home Manager module installs a real executable at `services.smalltalk.stateDir/bin/st3`
and provides `st` as a relative symlink there. Activation copies a changed build to a temporary
file in that directory and atomically renames it over `st3` before Home Manager restarts the
daemon. Identical contents leave the file's inode and modification time unchanged, so a no-op
activation does not make seats re-exec. Both the daemon and declaration-apply service use this
stable executable, and the service PATH puts its directory first; `pty` still comes from the
configured package. A systemd restart trigger or a build reference in the launchd plist makes
Home Manager restart the daemon when the package changes despite its stable executable path.

`scripts/st3-graceful-messaging-eval/run` proves this end to end for each harness; see
[the eval](../../evals/st3/graceful-messaging/README.md).
