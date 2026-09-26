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
  `st3 agents show` and the `agents` client resource;
- the reconciler's work wake and its retry deadline;
- `Store::seat_queue`, which feeds `st3 agents queue`;
- the client work list when it is filtered to one agent, so `st3 work ls --as AGENT` lists ready
  work in the same order;
- `work claim`, which refuses to let the seat take a later run's step first.

The existing nested-step rule is unchanged. A nested step whose listed parent step has the same
selector is reached through the parent and is not selected separately.

## Commands

```sh
st3 agents queue agent/fleet/example/worker
st3 agents queue move agent/fleet/example/worker mission-run/release/2026-09-26 --top \
  --reason "the release needs this first" --as person/operator
st3 agents queue move agent/fleet/example/worker mission-run/docs/2026-09-26 \
  --after mission-run/release/2026-09-26 --as person/operator
```

`st3 agents queue AGENT` prints the held step, the next work, each queued run in order with its
state (`claimed`, `ready`, or `waiting`), and the most recent moves, newest first:

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

`move` takes exactly one of `--top`, `--bottom`, `--before RUN`, or `--after RUN`. It needs person
authority from `--as person/NAME` or `person` in the st3 config, like other client-v0 mutations.
The run and any anchor must be queued for the seat. After a move, the command prints the new
queue. With `--json`, it prints the action result.

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
| `reason` | optional human reason |

The claim's actor is the person, and its accepted time is when the move happened. The claim kind
uses the `authorized-requester` write policy, so the raw public claim endpoint refuses it; the
dedicated `agent.queue-move` action writes it. A retry with the same idempotency key returns the
same claim.

No table stores the order. Each read derives it from two inputs:

1. joins from the `step_runs` projection: for each live run, the earliest step creation time for
   the seat;
2. moves, in graph order: accepted time, then writer, writer sequence, and store index.

`seat_queue::replay` applies joins in time order, with the run subject as a tie breaker. Each move
applies after every join recorded at or before its time. A move that names a run whose join time
is later, because the writer's clock ran ahead of the mover's, joins that run first; the mover
could only name a queued run. A move naming a run with no join, or an anchor that is not queued,
is ignored.

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
  - history names who moved what, why, and when; a retried move is one record; and a replica
    rebuilds the same order and history;
  - moves must name queued runs and valid anchors.
- `client_v0_contract::agent_queue_read_and_person_move_share_one_seat_order`: the read, the
  person action, the agent-filtered work list order, validation errors, and an unknown agent.
- `client_v0_cli::agents_queue_cli_shows_seat_order_and_records_person_moves`: human and JSON
  output, `--as`, and the configured person fallback against a temporary daemon socket.
- `tests::agent_queue_view_lists_the_claim_then_runs_in_order_and_moves`: the exact human view.
- `evals/st3/seat-queue`: a paid eval with one durable Claude seat and three runs. Held-out judges
  replay graph history at every claim. They require that a live agent took the first ready step in
  queue order each time, followed a person's move, passed over a head run waiting on a gate and
  returned to it once ready, kept each held claim, and got no terminal input. Run reports are in
  `evals/st3/seat-queue/reports/`.

## Left out or uncertain

- **Claims are refused out of run order, not out of step order.** The first live eval showed that
  the wake and the agent's work list are not enough. A Claude seat that finished one step checked
  its list, then claimed a later run's step before the wake for the head run arrived. `work claim`
  now refuses that. Inside one run, the mission's dependencies still decide. `available-to` work
  is not queued, so it is not refused. An agent that wants another run first needs a person to
  move it.
- **The boot contract lists fleet work.** Agents are told to run `st3 work ls` without `--as`. That
  list is in creation order, not seat order, and it includes steps the agent cannot claim. The
  claim check makes the seat order hold anyway. Changing the boot contract is left to its owner.
- **Only people can move runs.** Client-v0 mutations require person authority. An agent, such as
  a chief of staff, cannot reorder a seat without a new authorized path.
- **A reorder does not withdraw a wake that was already sent.** If the old head was woken and not
  yet claimed, the seat also receives a wake for the new head. Withdrawing the old wake would count
  as a closed attempt and could exhaust wakes when moves go back and forth.
- **Child runs are separate entries.** A run published by `work publish-mission` joins the queue
  on its own and is not grouped under its parent run.
- **Membership follows `assigned-to` only.** An `available-to` step never puts a run in a seat's
  queue, which matches the earlier selector.
- **Concurrent moves resolve by graph time.** Two people who move runs on different hosts at
  nearly the same time both take effect, in accepted-time order. The later one can undo the
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
