# Seat lifecycle

A seat's declaration, work, messages, and decisions live in the graph. A running harness also has its own native session. Restarting a process does not erase the plan; preserving the **exact native conversation** is what suspend/resume is for.

Use the worker declared in [missions in practice](missions-in-practice.md#declare-a-worker-seat). Check it before changing anything:

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

When repeated failed starts have parked a seat, an authorized person's or agent's explicit restart permits one launch attempt for that request. Replaying the request or restarting the daemon does not grant another attempt. If the retry fails, the seat parks again and its cause stays visible in `st agents show` and the agents view. A new restart request permits a new retry. Provider version checks and native-session continuation still apply.

The graph retains claimed work and pending mail. A relaunch continues the previous native session when the harness can; otherwise it starts a new session and recovers work from the graph. Stop and start explicitly when you want it off between uses:

```sh
st agents stop agent/garden/worker --as person/ada
st agents start garden/worker --as person/ada
```

Native drivers check their work once a minute and renew held claims only while the claim's
exact harness incarnation remains live, including an idle or blocked harness. Seats without
held claims do not read full subject status for lease maintenance. Checking work before
status keeps resident seats cheap without extending the renewal period or weakening the
incarnation check.

A restart of a seat that is up and ready first asks it for its handoff and waits at most two minutes
(`--now` skips the wait); see [seat handoff](st3/seat-continuity.md).

A restarted seat that cannot continue its conversation (another account, another model, a crash)
gets a first message with the handoff it left and the live state of its work. See
[seat handoff](st3/seat-continuity.md); a seat writes its handoff with `st agents handoff write`.

To preserve the exact conversation, wait until the seat has finished its turn and holds no claim:

```sh
st agents suspend agent/garden/worker --as person/ada --reason 'Idle until tomorrow.'
st agents show agent/garden/worker
st agents resume agent/garden/worker --as person/ada
```

Suspend refuses a busy seat with its blockers: a turn, claimed work, pending question, unsent input, or running subagent. A new harness may need one turn before it has a saved native session. Mail waits while suspended; it does not wake the seat. Resume verifies the same session on the same host. See [suspend/resume](st3/suspend.md) for failure reasons and limits.

## Client end reasons

Client-v0 agent rows optionally carry `end_reason`: a derived terminal classification,
not a new lifecycle write. Its `kind` distinguishes clean completion, a stop requested
by a person or agent, a crash, a diagnostic harness exit, a lost host, and retirement.
`ended_at` identifies the observed boundary; available actor, reason, exit code/signal,
and diagnostic accompany it.

Missing evidence remains absent. Completion requires a current ended harness or an
observed exited runtime, explicit exit 0, no signal, and no stop request. Terminal
harness diagnostics remain tied to that runtime incarnation after its exit; a newer
incarnation cannot inherit them. Unknown actors are not guessed. A stop request is
not an ended reason until termination is observed; confirmed suspension is separate
positive evidence. Confirmed owned-set retirement and actor-attributed stops remain
explainable on historical rows and take precedence over an otherwise clean exit.
Provider exit codes take precedence over wrapper outcome codes; failure in either
source prevents completion. Suspension evidence uses the same snapshot and retains
the original suspend actor and boundary when a different requester fails to resume.
Consumers preserve stale/missing observation states instead of displaying retained
completion evidence as current success.

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

Codex imports select the exact thread through st's strict resume handshake and pin the
originating `CODEX_HOME`. Discovery currently reads `$HOME/.codex/sessions`, including a
symlink to another home; sessions stored only in another home are not discovered yet
([#1495](https://github.com/compoundingtech/smalltalk/issues/1495)).

OMP imports use the same strict resume handshake, with `ST3_NATIVE_RESUME_SESSION`
holding the native session ID and `ST3_NATIVE_RESUME_PATH` pinning the absolute
transcript path. The driver validates the transcript header, links the transcript
and its artifacts into the managed inventory without copying them, and launches OMP with that exact
path. Missing or mismatched transcripts fail the launch rather than starting anew.
The pair bootstraps an imported or repaired seat until an incarnation-bearing
durable `harness.session-file` binding records a `desired_token` naming the
declaration that introduced or last changed the pair, or a canonically later
declaration carrying the same pair. The launcher stamps this token into
`ST3_LAUNCH_DESIRED_TOKEN`, and the native-session report persists it in the binding.
An incumbent's late binding carrying a pre-repair token cannot consume bootstrap.
Local start-observation trimming and replication to another node cannot rearm it.
Bindings from older drivers without this optional field never expire bootstrap;
they remain strict until an upgraded driver publishes a proven binding.
Unrelated declaration edits do not reset it. Subsequent restarts ignore the pair
and continue the latest bound native session by ID within the linked inventory.
Only strict import bootstrap uses its pinned transcript path. Explicit suspension
resumes and rollouts retain strict selectors.
Declaration/binding and refusal/binding precedence use canonical claim order,
so replication arrival order does not change which native session launches.

Ordinary continuation reads the linked managed inventory by native ID. If its
transcript is missing, the driver reports a visible `native-continue-unavailable`
warning with status `transcript-missing` and starts a fresh session. The refusal is
remembered permanently for that native ID, even after a newer binding. Subsequent
launches do not retry the refused continuation or the original import bootstrap,
so relocation does not cause a repeated refusal loop. A selector-scoped
`authored-session-selection` refusal can stop applying after the selector changes
or a canonically newer binding. Supporting relocation requires an
owned-inventory relink shared by reporting, ask recovery, suspension and rollout
consumers.

The daemon and native driver must both support strict OMP import paths. An older
driver with a new import declaration rejects the strict path contract; upgrade
both before importing.

### Repair an OMP import created before strict resume

Upgrading st does not migrate stored declarations. Re-importing an already-declared
seat does not replace its declaration. Stop the seat and export its current KDL:

```sh
st agents stop agent/import/omp/ID --as person/ada
st subject show agent/import/omp/ID --kdl --show-env-values > imported.kdl
```

Keep the identity, workspace, and other settings. Remove the harness's
`args "--resume=/absolute/transcript.jsonl"` selector and add:

```kdl
env {
  ST3_NATIVE_RESUME_SESSION "NATIVE_SESSION_ID"
  ST3_NATIVE_RESUME_PATH "/absolute/transcript.jsonl"
}
```

The native ID must match the transcript's session header. Apply the repaired
declaration and inspect the resulting seat:

```sh
st apply imported.kdl --as person/ada
st agents show agent/import/omp/ID
```

### Repair a Codex import created before strict resume

Upgrading st does not rewrite an existing imported seat's stored declaration. A Codex import
that still declares `args "resume" "NATIVE_SESSION_ID"` can continue failing with the
30-second typed thread-ownership timeout. Re-importing it does not repair the declaration:
the durable seat already exists.

Stop the affected seat and export its declaration for editing:

```sh
st agents stop agent/import/codex/ID --as person/ada
st subject show agent/import/codex/ID --kdl --show-env-values > imported.kdl
```

Keep the same seat identity, workspace and other settings. Remove the harness's `resume`
argument and its session ID, and add the strict selector and the home that owns the selected
rollout. `CODEX_HOME` names the home directory, not its `sessions` directory or rollout file:

```kdl
version 2
agent "import/codex/ID" {
  workspace "/work/garden"
  harness "codex" {}
  env {
    ST3_NATIVE_RESUME_SESSION "NATIVE_SESSION_ID"
    CODEX_HOME "/work/codex-origin"
  }
  restart "always"
}
```

Apply the repaired declaration and check the resulting seat:

```sh
st apply imported.kdl --as person/ada
st agents show agent/import/codex/ID
```

The resumed session keeps the settings it was saved with, such as its permission mode. To choose
them, declare the seat yourself with the same identity, workspace, and resume arguments, and keep
that file with your other seats. For a Claude Code session:

```sh
st agents start import/claude/ID --harness claude --workspace WORKSPACE \
  --model claude-sonnet-5 --effort medium \
  --arg=--resume --arg NATIVE_SESSION_ID --arg=--permission-mode --arg auto \
  --as person/ada --print-kdl > imported.kdl
st agents stop agent/import/claude/ID --as person/ada
st apply imported.kdl --as person/ada
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

Ordinary `st agents ls` and `st agents tree` read the latest published roster without
requesting a refresh. Immediately after starting or changing an agent they can show the
earlier roster; after daemon startup the roster may still be being prepared. When you need
to see your own write, pass its same-host receipt `store_index` as `min_cut` to the agents
client read and check the served snapshot index. This is not a replicated or cross-host cut.

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
prints; the owner's st daemon routes each tunnel straight to the PTY session. While that daemon is
stopped or restarting, as during every deploy, it refuses new tunnels and its live remote attaches
drop; `st terminals attach` reconnects once it is back. A host that needs attaches to work without
its daemon runs `st terminals expose-fabric --exec`, which has Fabric start a process per attach.
Otherwise it goes through the client gateway as your person, the same path the apps use. `st ui`
on a machine with its own st daemon attaches a terminal on this host to its PTY session, and one
on another fleet host over Fabric when Fabric reaches the owner; the daemon only says where it lives.
When Fabric cannot reach the owner, `st ui` goes through the client gateway and the terminal header
says so with Fabric's reason. An owner that refuses the incarnation is never worked around. A paired
device with no daemon of its own, such as the phone, uses the gateway.

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
st apply worker.kdl --as person/ada
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

Ordinary messages are `silent` or `wake` (the default for every sender and seat). Silent mail stays unread without waking; each real wake offers at most eight, with older mail available on demand. There is no per-seat wake policy or inferred question type. See [silent messages](st3/silent-messages.md).

## Pending runtime starts

Starts run on two dedicated launch workers, with one outstanding operation per seat. A blocked launch retains its slot; revocation does not free it until the backend call returns. Other reconciliation, physical stops and safety checks continue. When both workers are occupied, another start remains durably queued and completion wakes its admission.

`st agents show` displays `START` with the pending operation ID and queued, running, unknown, completed or revoked phase. `START JOBS` counts this owner's running launch calls and revoked calls still running. `START OPEN` counts unresolved operations, including revoked operations whose producer or runtime closure remains unverified after their launch call returned. These detail fields are local to the runtime owner. Revoked means authority was withdrawn; it does not mean the producer or runtime has ended. No launch input or environment is exposed in this view.

Stop requests revoke pending starts before physical control. An absent observation does not establish a completed stop while a launch remains unresolved. A stale or revoked completion cannot write a start-success or starting observation. If a revoked runtime publishes later, cleanup requires its recorded operation tag and exact incarnation; missing or conflicting provenance leaves the operation unresolved and raises a diagnostic. Same-name replacement identity never authorizes that cleanup. A successful native PTY stop supplies positive retirement proof only after its generation-fenced process tree and work scope have ended. The writer binds that proof to the original operation, resulting incarnation and exact terminate request/result receipts before releasing admission; it does not depend on a live PID remaining in the exited registry entry.

Returned launches remain unknown until their resulting incarnation and operation provenance are verified. Missing observations, conflicting tags and provenance lookup errors retain admission and evidence; they do not write a success receipt. Later exact verification completes only the original operation and declaration. Positive closure without a verified launch resolves as failure. Public action receipts carry writer-set `operation_status`: `verified` for an exact-incarnation start, `producer-closed` for a positively closed start failure, and `retired` for a positively verified exact termination. Each carries its immutable operation, runtime kind and ID, original desired token and exact request evidence; a verified launch also carries the resulting incarnation. Ordinary failure has no closure status, and a UUID or status label alone is not proof without those bindings.

This dispatch change does not guarantee that a detached PTY producer cannot publish after revocation. That needs a producer-side cancellation boundary. A returned error or an absent runtime is not proof that the producer closed. Native rejection before producer or exec child creation carries closure evidence bound to the exact operation; exec retirement requires its matching persisted generation and verified group exit. Errors after producer creation retain admission until exact closure or operation-owned retirement is proved. Unresolved records survive daemon restart without replaying the launch. Resolved ledger rows and their prepared launch bodies are also retained, so local disk use currently grows with launch count.


Ledger installation captures immutable publication format cuts in local `meta` before the first operation-bound request. Reopening, graph replay and view forgetting retain them. A used ledger missing either cut is refused: preserve the store and original boundary evidence for forward recovery rather than inventing a later boundary. Resolved ledger bindings and proof remain stored, but releasing their evidence pins allows original local request/result observation bodies to be trimmed. Consumers must use the exact owner ledger and request bindings, never UUID text alone.

When `START OPEN` stays nonzero after a launch error, publish an ordinary stop for the affected seat, inspect its operation ID and `start-revocation-unverified` diagnostic, and preserve the owner database and runtime records for Operations. Continue work on other seats. A later publication carrying that operation tag is retired using its exact incarnation. With an older PTY producer that never publishes, there is currently no supported command that proves producer closure and releases that seat: leave the stop and ledger intact and escalate to Operations. PID absence, elapsed time, daemon restart and deleting the ledger are not closure proofs. Shipping with this residual liveness limit requires the producer-contract decision; producer acknowledgment is a separate coordinated slice.

The additive ledger migration advances local storage to version 19. Before rollback, Operations must drain launch calls, revoke pending work and verify closure of every producer and operation-owned runtime. Preserve the upgraded database, ledger and runtime generation records as evidence, then restore a consistent pre-upgrade database and runtime state with their matching binary. New exec generation records contain operation provenance that older runtimes cannot read; restoring only SQLite is insufficient. Never downgrade the header in place or use a claim-only backup to erase unresolved cancellation authority. If closure cannot be established, roll forward.

### Runtime records and a consistent capture

Resolve `OPS_STATE_DIR` and `OPS_PTY_ROOT` from each member's actual service arguments and config, and record them in a private Operations manifest with the exact installed binary SHA. The default state directory is `${XDG_STATE_HOME:-$HOME/.local/state}/st3`; `--state-dir` or config can override it. The default PTY root is `OPS_STATE_DIR/pty`; `--pty-root` or config can override it. Keep member names and resolved home paths out of public documents.

| Evidence | Exact location |
|---|---|
| Database, including the operation ledger | `OPS_STATE_DIR/claims.sqlite3`; committed data may also be in `claims.sqlite3-wal` before backup |
| Current and previous exec generations | `OPS_STATE_DIR/exec/<runtime-id>.json` and `<runtime-id>.json.1` |
| PTY generation, operation tags and registry identity | `OPS_PTY_ROOT/<runtime-id>.json`, `<runtime-id>.pid`, and `.recovery/<runtime-id>.revision.json` |
| PTY spawn work-scope record and lock file | `<canonical-pty-root-parent>/.st3-pty-spawn-locks/<root-hash>/<runtime-hash>.scope` and `.lock` |

`root-hash` is lowercase SHA256 of the canonical absolute PTY root's raw Unix path bytes, with symlinks resolved, no trailing separator and no newline. `runtime-hash` is lowercase SHA256 of the runtime ID's UTF-8 bytes, with no newline. The lock-tree parent is the **canonical** root's parent, which may differ from the configured parent when a symlink is used. Existing roots must be readable directories; do not guess the digest of a missing root. Lock files contain no persisted kernel lock ownership and are not closure proof.

The version-19 roll is forward-only. Operations' routine pre-roll backup remains the online SQLite snapshot; it does not supply a swap-back cut. The following consistent runtime capture stops every owned seat and the daemon on one member. It is a planned full-outage maintenance procedure only if the people responsible request a swap-back option and accept that window. Do not require this outage as part of an ordinary roll.

Before either a requested pre-upgrade rollback cut or an emergency restoration capture:

1. Record the member's exact binary SHA, resolved roots and a reviewed, explicit JSON array of owned runtime IDs. On that owner's daemon, run `st agents ls --all --json`, follow every returned cursor with `--cursor`, and retain all pages. Use each local agent's runtime ID, plus the exact runtime IDs of operations being closed and declared exec runtimes from the owner inventory. The roster is the latest publication, not a synchronous inventory: after startup it may be unavailable and after a write it may lag. A same-host receipt cut fences only graph publication, not the completeness of the service/process inventory. Historical agent rows are not proof of live writers. Supplement this with Operations' service/process inventory so an undeclared but live producer/runtime cannot be omitted. An unlisted live writer makes the capture inconsistent and prohibits swap-back. Resolve every unknown producer and runtime; an incomplete inventory prohibits swap-back. Do not enumerate or copy the exec/PTY trees to construct this list.
2. Withdraw launch authority, stop each owned runtime through ordinary incarnation-fenced controls, and obtain the per-operation evidence below while the daemon can still reconcile. `START JOBS=0` and `START OPEN=0` are necessary checks for modern operations, not closure proof. If a producer or runtime cannot be proved closed, preserve the store and roll forward. Stopping the daemon or losing a PID does not supply that proof.
3. Quiesce the daemon under its service manager only after closure is established. Keep it stopped and prevent any listed runtime/producer from writing its records throughout the capture. If a writer cannot be quiesced, do not take a purportedly consistent capture.
4. Take a SQLite backup and copy only the named generation/scope files into one new private archive, recording each listed file's presence or absence. The following Python 3.9-compatible recipe uses point lookups, not directory walks. A SQLite backup includes committed WAL contents; copying `claims.sqlite3` alone while a WAL exists is insufficient. The archive does not contain live sockets, processes or kernel locks.

```sh
# Set these from the private member manifest; inventory is an explicit JSON array.
umask 077
python3 - "$OPS_STATE_DIR" "$OPS_PTY_ROOT" "$OPS_CAPTURE_DIR" "$OPS_RUNTIME_IDS" <<'PY'
import hashlib, json, os, shutil, sqlite3, stat, sys
from pathlib import Path

state, pty, capture, inventory = map(Path, sys.argv[1:])
pty_bytes = os.path.realpath(os.fsencode(pty))
if not os.path.isdir(pty_bytes):
    raise SystemExit("PTY root is missing or unreadable; capture refused")
spawn = Path(os.fsdecode(os.path.join(
    os.path.dirname(pty_bytes), b".st3-pty-spawn-locks",
    hashlib.sha256(pty_bytes).hexdigest().encode("ascii"))))
runtime_ids = json.loads(inventory.read_text())
if not isinstance(runtime_ids, list) or any(
    not isinstance(i, str) or not i or i in (".", "..")
    or "/" in i or "\\" in i or "\0" in i for i in runtime_ids
):
    raise SystemExit("inventory must contain explicit runtime basenames")
source = sqlite3.connect(state.joinpath("claims.sqlite3").resolve().as_uri() + "?mode=ro", uri=True)
# Account for committed WAL pages and reserve room for both recovery cuts.
database_bytes = source.execute("PRAGMA page_count").fetchone()[0] * source.execute("PRAGMA page_size").fetchone()[0]
margin = max(1024 ** 3, database_bytes // 10)
required_bytes = 2 * database_bytes + margin
# OPS_CAPTURE_DIR's parent must already exist on the intended archive filesystem.
# Preserve the checked margin there; do not allocate this member's last free space.
available_bytes = shutil.disk_usage(capture.parent).free
if available_bytes < required_bytes:
    raise SystemExit("insufficient free space for two database cuts plus margin; capture refused")
capture.mkdir(mode=0o700, exist_ok=False)
target = sqlite3.connect(str(capture / "claims.sqlite3"))
source.backup(target)
target.close()
(capture / "claims.sqlite3").chmod(0o600)
source.close()
files = []
for runtime in sorted(set(runtime_ids)):
    digest = hashlib.sha256(runtime.encode("utf-8")).hexdigest()
    candidates = [
        ("exec", state / "exec", runtime + ".json"),
        ("exec", state / "exec", runtime + ".json.1"),
        ("pty", pty, runtime + ".json"),
        ("pty", pty, runtime + ".pid"),
        ("pty", pty, ".recovery/" + runtime + ".revision.json"),
        ("spawn", spawn, digest + ".scope"),
        ("spawn", spawn, digest + ".lock"),
    ]
    for role, root, name in candidates:
        original = root / name
        entry = {"role": role, "name": name}
        try:
            mode = original.lstat().st_mode
        except FileNotFoundError:
            entry["present"] = False
        else:
            if not stat.S_ISREG(mode):
                raise SystemExit("non-regular record; capture refused: " + str(original))
            destination = capture / role / name
            destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            shutil.copyfile(original, destination)
            destination.chmod(0o600)
            entry.update(present=True, sha256=hashlib.sha256(destination.read_bytes()).hexdigest())
        files.append(entry)
(capture / "runtime-manifest.json").write_text(json.dumps({
    "state_dir": str(state.resolve()), "pty_root": os.fsdecode(pty_bytes),
    "spawn_root": str(spawn), "runtime_ids": sorted(set(runtime_ids)), "files": files,
}, indent=2) + "\n")
PY
```

Operations adds the closure evidence, service-quiescence evidence and exact binary SHA to this archive and verifies the backup before restarting anything. An exception or partial archive is not a recovery cut. For swap-back, first preserve the upgraded cut using the same recipe, then restore the pre-upgrade database and its matching listed runtime files together while writers remain quiesced. Honor the manifest's recorded absence only for its explicitly inventoried paths; do not remove unrelated files or recursively replace a runtime tree. If a post-upgrade operation touched an ID absent from the pre-upgrade inventory, or a prior file is not captured, the preimage is unproved: preserve and roll forward. No live process may be reconstructed from an archived PID or socket.

### Check closure per operation

Inspect each exact operation on its owner, using a read-only database connection. Substitute its recorded ID for `OPERATION_ID`; this primary-key lookup exposes bindings and proof without dumping launch environment or input:

```sh
python3 - "$OPS_STATE_DIR" "$OPERATION_ID" <<'PY'
import json, sqlite3, sys
from pathlib import Path
connection = sqlite3.connect(
    Path(sys.argv[1]).joinpath("claims.sqlite3").resolve().as_uri() + "?mode=ro", uri=True)
connection.row_factory = sqlite3.Row
row = connection.execute("""
SELECT id, subject, owner, dispatched, finished, applied, cleanup_verified,
       revoked_by,
       json_extract(body,'$.selected_token') AS selected_token,
       json_extract(body,'$.desired_token') AS desired_token,
       json_extract(body,'$.member.runtime_id') AS runtime_id,
       json_extract(body,'$.member.terminal') AS terminal,
       json_extract(body,'$.start_request') AS start_request,
       json_extract(result,'$.operation_id') AS result_operation,
       json_extract(result,'$.verification') AS verification,
       json_extract(result,'$.producer_closed') AS producer_closed,
       json_extract(result,'$.incarnation') AS incarnation,
       json_extract(result,'$.closure_proof_id') AS closure_proof_id,
       json_extract(result,'$.closure_proof') AS closure_proof,
       json_extract(result,'$.result_receipts') AS result_receipts,
       json_extract(result,'$.retirement_request') AS retirement_request,
       json_extract(result,'$.retirement_result') AS retirement_result
FROM local_runtime_starts WHERE id=?;
""", (sys.argv[2],)).fetchone()
if row is None:
    raise SystemExit("operation not found; closure unproved")
print(json.dumps(dict(row), indent=2))
connection.close()
PY
```

Operations must verify the retained owner, subject, runtime ID/kind, original selected declaration, exact start request and result bindings, then record which positive proof applies:

- A positively closed producer has `finished=1`, `applied=1`, `cleanup_verified=1` and `producer_closed=true`. A typed `producer-closed` result binds it to the exact operation. A revoked operation never dispatched to a worker carries a retained `closure_proof` with witness `never-dispatched`; no runtime was created by that operation. Any prior runtime still needs its own retirement check.
- An already-ended exact operation has `verification=operation-ended` and a retained closure proof naming its actual incarnation and ending status, distinct from the prior `expected_incarnation`. The owner verifies matching physical operation provenance before writing this proof. An ending observation or cleanup flag without that binding is insufficient.
- A verified retirement has `verification=operation-retired`, `finished=1`, `applied=1`, `cleanup_verified=1`, a resulting incarnation, and a retained `closure_proof` with witness `verified-native-retirement`, the full original request snapshots, exact terminate-request snapshot and result identity. `retirement_request`/`retirement_result` identify public receipts when their original evidence is still retained; withdrawal of original evidence suppresses publication but leaves the authoritative retained positive proof. Their proof binds both the original start request and the exact terminate request to that operation and incarnation. Receipts may have trimmed; use the permanent owner ledger proof and bindings, not UUID text or a caller label.
- For exec closure backed by generation records, the current or previous `.json` must bind `start_operation` to the exact operation and `generation_id` to its resulting incarnation, with verified `group_exited=true`. Verify retirement of its recorded `scope_unit` as well when present; for a Linux user scope, `systemctl --user show --property=ActiveState --value UNIT` must establish inactivity. On a launchd member without a scope unit, the matching generation and verified `group_exited` are the exec closure evidence; do not invent a systemd check there. Preserve the generation and scope evidence. PID absence or a missing generation file alone is insufficient.

`verification=operation-incarnation` proves a launch, not runtime closure. Ordinary failure, `launch-accepted-unverified`, a cleanup flag without matching positive evidence, zero aggregate counts, missing registry entries and old PTY errors do not prove closure. If a historical operation has no retained positive retirement/closure proof and cannot be matched to a verified retired generation, mark it unproved and refuse swap-back; this slice adds no general closure or operator-release command. In particular, an old PTY launcher that errors after starting and never publishes leaves its seat busy with **no supported release command**. The cancel-before-start producer protocol and this residual limitation were approved for this slice; the closure command is deferred. Operations still refuses a swap-back without positive closure proof.

### Measure retained-ledger growth

`local_runtime_starts` lives inside each owner's `OPS_STATE_DIR/claims.sqlite3`, with uncheckpointed updates possibly in `claims.sqlite3-wal`. It retains one row for every durably admitted/queued start, including resolved, failed and revoked starts; seat-busy and superseded refusals add no row. No rows or prepared bodies are pruned. The capture requires free space for two full database cuts plus a margin of at least 1 GiB or ten percent of the committed database size, whichever is larger; large member stores require a planned capacity check. Payload includes immutable original physical/control request snapshots, typed closure proofs when resolved by closure, and the full authoritative runtime observation plus canonical sortable key for verified starts. Payload size varies with prepared input/environment and results, so there is no fixed bytes-per-launch bound.

At an Operations capacity review, measure the retained row count and logical payload bytes (this intentional offline aggregate reads the ledger, not the exec directory):

```sql
SELECT COUNT(*) AS operations,
       COALESCE(SUM(length(CAST(body AS BLOB))
                    + COALESCE(length(CAST(result AS BLOB)),0)),0) AS payload_bytes
FROM local_runtime_starts;
```

Compare two timestamped samples: `delta(operations)/elapsed` is the admission rate and `delta(payload_bytes)/elapsed` is the measured payload growth rate. Include SQLite row/index/page overhead and other tables by separately tracking the database plus WAL disk allocation. Begin with the first deployment member and add the other member-specific manifests; do not infer an unmeasured fleet-wide rate. Evidence pins can be released, but this retained ledger continues to grow.

Operations rolls forward-only storage changes separately: deploy the first merged storage upgrade alone, verify it and settle that cut, then deploy the other as its own cut. Do not combine the version-19/version-20 upgrades in one roll. Operations confirmed that production members never ran these unreleased draft builds; if a draft-opened store is discovered, preserve it and roll forward only.
