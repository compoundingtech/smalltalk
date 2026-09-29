# Agent seat queues

This document explains the seat queue for reviewers: the model, the commands, how the order is
stored and replicated, how it is tested, and what is left out or uncertain.

## Model

An agent seat holds at most one claim at a time. A claimed, working, or verifying step occupies
it. Before this change, a free seat was woken for the oldest ready step across all runs, ordered
by step creation time and subject.

Now each seat has one ordered queue of mission runs:

- A run joins the end of the seat's queue when it first has a step assigned to that seat.
- A run stays queued while its current generation has a step for the seat. A revision keeps the
  run's place, because the join time is the first such step in any generation.
- A run leaves the queue when it is terminal.
- The seat's next work is the first ready step, in queue order, that is assigned to the seat.
- A run with no ready step for the seat is passed over. It may be waiting on a gate, a dependency,
  or another seat. The next run's ready step is taken instead, and the first run is next again as
  soon as it has a ready step.
- Inside one run, `depends-on` and `queue {}` still decide readiness. Ready steps of one run keep
  creation order, then subject.
- A move never releases, reassigns, or interrupts a held step. It only changes which run is next
  once the seat is free.
- A revision does not take a held step's place either. A revision carries a claimed step into the
  new generation as a ready step with a new subject, so the seat must claim it again. Until it
  does, that step is the seat's next work, ahead of any earlier run's ready step, and the seat is
  woken for it. If the seat releases the step after claiming it again, the step waits its turn in
  queue order like any other.
- `work claim` refuses a ready step assigned to the seat when an earlier run in the queue has a
  ready step for the seat. The error, `seat-queue-order`, names that next work. Claims inside one
  run, nested steps reached through their parent, and `available-to` work are not refused.

The queue matters most for a durable top-level seat that serves many runs. A mission-scoped seat
normally serves one run, so its queue has one entry.

## One selector

`crates/st3/src/seat_queue.rs` holds the only next-work selector, `seat_queue::select`. It replaces
the two oldest-first orderings that existed before, one in `Store::agent_work_queues` and one in
the reconciler's `next_work_wake_for_agent`. It is now used by:

- `Store::agent_work_queues`, which feeds `NEXT WORK`, `QUEUED WORK`, and `UPCOMING` in
  `st agents show` and the `agents` client resource;
- the reconciler's work wake and its retry deadline;
- `Store::seat_queue`, which feeds `st agents queue` and its alias `st missions queued`;
- the client work list when it is filtered to one agent, so `st work ls --as AGENT` lists ready
  work in the same order;
- `work claim`, which refuses to let the seat take a later run's step first.

A nested step whose listed parent step has the same selector is reached through the parent and is
not selected separately. The exception is a parent that the seat submitted while that nested step
was still ready. That parent no longer holds the seat, and the nested step is selected as its own
run's ready work, in queue order. `work claim` then refuses it while an earlier run has ready work
for the seat, like any other step of its run.

## Commands

```sh
st agents queue agent/fleet/example/worker
st agents queue move agent/fleet/example/worker mission-run/release/2026-09-26 --top \
  --reason "the release needs this first" --as person/operator
st agents queue move agent/fleet/example/worker mission-run/docs/2026-09-26 \
  --after mission-run/release/2026-09-26 --as person/operator
```

`st agents queue AGENT` prints the held step, the next work, each queued run in order with its
state (`claimed`, `ready`, or `waiting`), and the most recent moves, newest first. `st missions
queued AGENT` runs the exact same show through the same code and prints the identical output,
including `--json`; there is no `missions queued move`, only `agents queue move`.

```text
AGENT QUEUE  agent/fleet/example/worker
CURRENT      step-run/build-7/build
NEXT WORK    step-run/docs-3/review
RUNS         3
  1. mission-run/build-7  claimed  step-run/build-7/build
  2. mission-run/ship-2  waiting  step-run/ship-2/ship not ready
  3. mission-run/docs-3  ready  next step-run/docs-3/review
MOVES        1 total
  2026-09-26T10:03:00.000Z  person/operator moved mission-run/build-7 before mission-run/ship-2: finish the build first
```

`move` takes exactly one of `--top`, `--bottom`, `--before RUN`, or `--after RUN`. The run and any
anchor must be queued for the seat. After a move, the command prints the new queue.

A person moves runs with `--as person/NAME` or `person` in the st config, like other client-v0
mutations. With `--json`, it prints the action result.

An agent moves runs with `--as agent/PATH` when a person has granted it that authority in its
declaration, the way `mission-authority` grants named missions:

```kdl
agent "fleet/example/chief" {
  workspace "."
  harness "claude" {}
  queue-authority {
    move "fleet/example/worker"
    move "fleet/review/*"
  }
}
```

Each `move` rule names an exact seat identity or a terminal `/*` namespace, without `agent/`. A
seat has no authority over its own queue unless a rule names it. The daemon reads the grant from
the agent's current desired declaration when the move arrives, and refuses a move outside it with
`queue-authority-denied`, or `missing-agent-queue-authority` when the agent has no declaration.
An agent cannot grant itself the authority: a top-level agent declaration that an agent publishes
is refused with `agent-authority-grant-denied` when it carries `queue-authority`,
`mission-authority`, or `seat-authority`. Agents may declare or stop a top-level seat only when a
person grants `seat-authority { declare "NAMESPACE/*"; stop "NAMESPACE/*" }` in the agent's
current declaration. A seat with authority cannot be re-declared by an agent, because that would
remove its person's grant. A person-declared top-level seat named `fleet/PROJECT/...` also holds
mission authority for `fleet/PROJECT/*` by default, and loses it while an agent's declaration of the
seat is current ([agent mission authority](kdl-lifecycle.md#agent-mission-authority)).
The agent's move goes to `POST /v1/agent-queue-moves`, because client-v0 actions carry only
person authority. That route also accepts a person. With `--json`, it prints the move claim.

The typed client exposes the same surface:

- read `agent-queue.get`: `GET /v1/client/agent-queues/{agent_id}` returns an `AgentQueue`;
- action `agent.queue-move` with `AgentQueueMoveParameters` (`agent_id`, `mission_run_id`,
  `placement`, optional `anchor_run_id`, optional `reason`) under the `control.work` capability.

The Rust (`Client::agent_queue`, `Client::agent_queue_move`), Swift (`agentQueue`,
`agentQueueMove`), and TypeScript (`agentQueueGet`, `agentQueueMove`) clients are regenerated from
the schema and operations manifest. Queue validation errors reach clients as
`validation-failed`, with the specific message and details kept.

## Storage and replication

A move writes one `agent.queue.moved` claim on the agent subject:

| Field | Meaning |
|---|---|
| `run` | the moved `mission-run/…` subject |
| `placement` | `top`, `bottom`, `before`, or `after` |
| `anchor` | the other queued run, only for `before` and `after` |
| `reason` | optional reason |

The claim's actor is the person or the agent that moved the run, and its accepted time is when the
move happened. The claim kind uses the `authorized-requester` write policy, so the raw public claim
endpoint refuses it; only the `agent.queue-move` action and the agent queue move route write it. A
retry with the same idempotency key returns the same claim.

No table stores the order. Each read derives it from two inputs:

1. joins from the `step_runs` projection: for each live run, the earliest step creation time for
   the seat;
2. moves, in graph order: accepted time, then writer, writer sequence, and store index.

`seat_queue::replay` applies joins in time order, with the run subject as a tie breaker. Each move
applies after every join recorded at or before its time. A move that names a run whose join time
is later, because the writer's clock ran ahead of the mover's, joins that run first; the mover
could only name a queued run. A move naming a run with no join, or an anchor that is not queued,
is ignored.

A revision that drops a seat's claim records the seat as `claimant` on the new step's
`step-run.carried` claim. A ready step is first for that seat while no `work.claimed` claim on the
step follows the carried claim. Each work read looks this up once for its ready steps, starting
from those steps, so the cost follows current work rather than every carried step in history. On
the performance fixture, all current work reads in 1.28 ms against 1.22 ms without the lookup,
and one seat's work in 0.30 ms against 0.29 ms.

The claims replicate like other agent claims, and the join times come from replicated run
claims, so every replica computes the same order. Terminal runs are read only when a move names
them. A run that never receives or anchors a move cannot change the relative order of the
others.

## Tests

- `seat_queue::tests`: replay order, each placement, writer clock skew, ignored moves, and nested
  steps.
- `reconcile::tests::seat_queue_*`: these tests use a real store, runs, claims, and reconciler
  passes. Each checks the queue view, the roster's next work, and the reconciler wake together:
  - three runs for one seat keep start order by default, and a terminal run leaves;
  - a move to the top changes the next work and the wake, and the promoted step is claimed and
    completed next;
  - the seat falls through a waiting head run and returns to it once another seat's step
    completes;
  - a held claim stays claimed after a move, the seat is not woken for other work, and a second
    claim still fails with `agent-capacity`;
  - a claim from a later run fails with `seat-queue-order` and names the next work, and succeeds
    after a person moves that run to the top;
  - a claim passes over a head run that has no ready step;
  - a revision of a later run whose step the seat holds, while an earlier run has ready work,
    keeps the carried step first: it is the next work and the wake, the seat claims it again
    without `seat-queue-order`, and the earlier run is next once it is done;
  - a carried step the seat claims again and then releases waits its turn behind the earlier run;
  - history names who moved what, why, and when; a retried move is one record; and a replica
    rebuilds the same order and history;
  - moves must name queued runs and valid anchors.
- `client_v0_contract::agent_queue_read_and_person_move_share_one_seat_order`: the read, the
  person action, the agent-filtered work list order, validation errors, and an unknown agent.
- `api::tests::an_agent_moves_a_seat_queue_only_with_queue_authority`: an agent granted the seat
  moves a run and is recorded as the move's actor in the queue view and history. The seat itself,
  an agent without a grant, an agent granted another seat, an undeclared agent, and a daemon actor
  are refused, and the order does not change. An agent cannot publish a top-level declaration that
  grants queue or mission authority to itself or another seat, and can still publish one without.
  A person can use the same route. A replica rebuilds the same order and history from the agent's
  and the person's moves.
- `mission::tests::agent_queue_authority_uses_exact_and_terminal_seat_rules`: exact and namespace
  rules, `${ST_MISSION_RUN}` in a rule, and refused empty, prefixed, wildcard, duplicate, and
  unknown rules.
- `client_v0_cli::agents_queue_cli_shows_seat_order_and_records_person_and_agent_moves`: human
  and JSON output for person and agent moves, `--as`, the configured person fallback, a refused
  seat, and a malformed actor, against a temporary daemon socket.
- `tests::agent_queue_view_lists_the_claim_then_runs_in_order_and_moves`: the exact human view.
- `evals/st3/seat-queue`: a paid eval with one durable seat and three runs. The seat is Claude
  `claude-sonnet-5` by default; the runner can put Codex `gpt-6-luna` or omp
  `openai-codex/gpt-5.6-luna` in it instead. A model-free chief agent with queue authority over the
  seat makes the move, after the seat's own move is refused. Held-out judges replay graph history
  at every claim. They require that a live agent took the first ready step in queue order each
  time, followed the chief's move, passed over a head run waiting on a gate and returned to it
  once ready, kept each held claim, and got no terminal input. Run reports are in
  `evals/st3/seat-queue/reports/`. Reports before the chief was added used a person's move.
  Of the eleven person-move runs, ten passed on Claude, Codex, and omp seats. In the other, a
  Claude seat never reached idle before any work existed.
  With the chief's move, both Claude runs and both Codex runs passed. One omp run passed; the
  other was void after its seat re-declared itself, described below, and its replacement passed.
  `scripts/st3-seat-queue-eval/report` drafts a run's report from the evidence the runner keeps.

## Left out or uncertain

- **Claims are refused out of run order, not out of step order.** The first live eval showed that
  the wake and the agent's work list are not enough. A Claude seat that finished one step checked
  its list, then claimed a later run's step before the wake for the head run arrived. `work claim`
  now refuses that. Inside one run, the mission's dependencies still decide. `available-to` work
  is not queued, so it is not refused. An agent that wants another run first needs a person, or
  an agent with queue authority for the seat, to move it.
- **`st work ls` without `--as` lists fleet work.** That list is in creation order, not seat
  order, and it includes steps the agent cannot claim. The claim check makes the seat order hold
  anyway, and the seat's next step also arrives as a message that names it.
- **Agents move runs only with a declared grant.** A person grants `queue-authority` in the
  agent's declaration, and the daemon checks it on each move, as it checks `mission-authority`.
  As there, the local socket trusts the actor named by `--as`, so the grant keeps well-behaved
  agents in bounds and is not a security boundary against local processes. Removing a grant stops
  later moves and leaves earlier ones in place. Paired and typed clients have no agent path;
  client-v0 actions stay person-only.
- **An agent once re-declared its own seat.** In one live eval run an omp seat ran
  `st agents start` for its own identity, as itself. The daemon accepted the declaration, which
  replaced the eval's: it dropped the model and the audit environment and set `restart always`.
  Eval cleanup then no longer owned the seat and could not remove its terminal, so the run was
  void although every judge passed. The later `seat-authority` check refuses this unless a person
  grants the agent permission to declare that seat.
- **A reorder does not withdraw a wake that was already sent.** If the old head was woken and not
  yet claimed, the seat also receives a wake for the new head. Withdrawing the old wake would count
  as a closed attempt and could exhaust wakes when moves go back and forth.
- **Child runs are separate entries.** A run published by `work publish-mission` joins the queue
  on its own and is not grouped under its parent run.
- **Membership follows `assigned-to` only.** An `available-to` step never puts a run in a seat's
  queue, which matches the earlier selector.
- **Concurrent moves resolve by graph time.** Two people or agents who move runs on different hosts
  at nearly the same time both take effect, in accepted-time order. The later one can undo the
  earlier one's intent. There is no fence on the agent subject, because harness claims change it
  constantly.
- **No projection table.** Reads replay moves on each call. That is cheap while moves are human
  actions. A seat with thousands of moves would need a cached projection. The reconciler reads a
  seat's order only when the seat holds nothing, has ready work in more than one run, and can be
  woken. [Seat queue performance](seat-queue-performance.md) measures the idle cost against the
  base commit.
- **The Swift client was not compiled.** The codegen checks that every Swift model field exists.
  The TypeScript client type-checks and its contract test passes.
- **Generated client drift was fixed.** `messages_list_for_peer` had been added only to the
  generated Rust client. The template now carries it, so regeneration keeps it.
