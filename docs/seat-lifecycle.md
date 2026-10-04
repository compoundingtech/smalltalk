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

## Bring an existing native session under st

A session started directly in a harness can be imported. Find it and inspect the exact identity and workspace before importing:

```sh
st import ls
st import ls --all
printf 'Exact external session ID to import: '
read -r native_session
st import show "$native_session"
st import run "$native_session" --as person/ada
st agents ls
```

Import stops an exactly identified running process, if present, and resumes its native session as a durable `agent/import/...` seat. Saved sessions appear with `--all`. If a running process cannot be identified exactly, exit it first and import its saved session. Do not import a session already owned by another seat.

**Claude import limitation:** resuming an imported Claude conversation currently does not load the Small Talk message channel. Use a declared Claude seat for messages and mission work. Check [native imports](../README.md#bring-in-sessions-st-does-not-own) for the current harness-specific limits.

## Spot and recover a stale or stopped seat

```sh
st agents ls --all
st agents show agent/garden/worker --json
st subject history agent/garden/worker --limit 20
st doctor
```

Read the selected declaration, latest runtime, and stop/suspension reason together. A seat hidden from the default list can still be shown by its exact ID. A `stale` delivery path means its message channel is not proving readiness; `outdated` means it is running older code. Check the local host's doctor report and any login/trust prompt before restarting. Resume a suspended seat; start an intentionally stopped one; restart a stale running one after fixing the cause.

Daemon restarts adopt running seats. Current drivers and channels follow a replaced binary without restarting the provider session; [seats across deploys](st3/seat-deploys.md) explains their `current`, `outdated`, `legacy`, and `stale` reports. See [troubleshooting](when-something-is-wrong.md) when a seat stops again.
