# Subagents

A subagent is a helper that a seat's harness runs inside its own session: a Claude subagent
started with the Agent tool, or a Codex subagent thread. It is not a seat or a mission. st records
each subagent as claims on its parent seat, from the harness's hooks and events, and never asks the
agent about it.

## Claims

| Claim | Recorded when | Fields |
| --- | --- | --- |
| `subagent.appeared` | the harness starts the subagent | `subagent_id`, `subagent_type`, `description` (one line), `driver`, `session_id`, `incarnation_id`, `step_run` (the step the seat held), `started_at_unix_ms`, `lease_expires_at_unix_ms` |
| `subagent.renewed` | half of a running subagent's lease has passed | `subagent_id`, `incarnation_id`, `lease_expires_at_unix_ms` |
| `subagent.ended` | the subagent ends | `subagent_id`, `outcome`, `reason`, `ended_at_unix_ms`, `duration_ms`, and its token buckets |

Each subagent gets one appearance and one end. A renewal is recorded every five minutes, and only
while the subagent runs. The subagent ID is the harness's own and is unique within its parent seat.
Prompts and transcripts stay on the host.

The store keeps each record whole. A repeated appearance or end answers with the claim already
recorded. A renewal or end of a subagent that never appeared is refused as `unknown-subagent`, and a
renewal after the end as `subagent-ended`.

## Where they come from

**Claude.** Every Claude seat's settings register `SubagentStart`, `SubagentStop` and `SessionEnd`
hooks beside its other hooks. The hooks keep a ledger, `harness-subagents`, beside the seat's other
harness records:

- `PreToolUse` for the Agent tool gives the launch's description and type.
- `SubagentStart` starts the subagent.
- `SubagentStop` ends it as `completed`. A stop for an agent that never started is Claude's trailing
  phantom stop, and it changes nothing.
- Each top-level `Stop` lists the background subagents still running. One missing from that list
  for ten seconds ends as `interrupted`.
- A new session or `SessionEnd` ends the session's subagents.

A running seat keeps the settings it started with, so it records subagents from its next restart.

**Codex.** The control connection reads the parent thread's `subAgentActivity` items and
`collabAgentToolCall` results into the same ledger. A subagent thread can take a follow-up task
after it completes, so each task is one run: `THREAD`, then `THREAD#2`, and so on. A run starts
when its thread starts or takes a task while idle. It ends when the parent hears it completed or was
interrupted, or when a collab call reports it errored, shut down or gone.

The seat's driver reads the ledger every second and records what changed. A subagent leaves the
ledger only after its end is recorded, so a daemon outage delays these claims without losing any.

## Lease and ends

A subagent's lease lasts ten minutes, and the driver renews it at half its length while the
subagent runs. The outcomes are:

- `completed`, `failed` and `interrupted`, as the harness reports them;
- `expired`, when the lease ran out without a renewal;
- `session-ended`, when the parent session changed or ended;
- `harness-exited`, when the harness exited or restarted;
- `seat-stopped`, when the seat was stopped or removed.

The driver records the ends the harness reports, the ends of a changed or ended session, and every
running subagent when its harness exits. A new driver incarnation ends what the previous harness
left running. A driver replaced in place keeps the same incarnation, so its subagents run on.

The reconciler's `stage/subagents` ends the rest. It runs on the node that recorded the appearance,
and it ends a subagent when the seat was stopped or removed, when its harness exited or restarted
under a new incarnation, or when its lease ran out. It starts each pass after the oldest subagent
still open and arms a timer for the next lease to run out. On every other node, reads show an open
subagent past its lease as expired.

## Tokens

The end carries the subagent's own token buckets, which are disjoint in the same way as
`harness.usage`.

- **Claude:** they come from the subagent's transcript, counting the last line of each response.
  Claude's subagent responses already reach the parent's usage at the parent's `Stop`.
- **Codex:** they come from the subagent thread's rollout, two seconds after the run ends. The
  driver takes the newest token total and subtracts what earlier runs of the same thread counted.
  It adds the result to the parent's harness timeline as one response with the parent's account,
  so `st usage` includes it.

## Where they show

Every agent resource in the client API carries `subagents`: the ones running now, which have not
ended and whose lease runs past the read. The read starts from the leases, through their indexes,
so it costs what runs rather than all history, and a lease that runs out leaves the list at the
next read without a claim. `st agents show` prints a `SUBAGENT` line for each, with what it
does, its type and when it started. `st agents tree` hangs them beneath their agent. stui shows
them beneath the agent's row in the agents list and tree and in the details panel. They are part
of the agent there: they select and click as the agent and have no actions of their own.

## Tests

`scripts/st3-subagents-eval/run` runs an isolated daemon with stand-in Claude seats that report
subagents through the real hooks, with no model. `crates/st3/tests/subagents_seat.rs` runs it. It
covers:

- appear then end, Claude's phantom stop, and many subagents at once;
- an interrupted subagent and a session that ended;
- a daemon restart in between;
- a frozen seat whose lease runs out, and a killed seat that its restart closes;
- a killed harness, a stopped seat, and a removed seat.

`ST3_SUBAGENT_LEASE_MS` can only shorten the driver's lease, so these tests need not wait out ten
minutes.
