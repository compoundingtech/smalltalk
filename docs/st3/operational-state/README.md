# st3 operational-state contract

Status: contract and red-fixture baseline. This document defines selection semantics; it does not
implement them.

## Three layers

Every operational read must name one of three layers. Mixing them is a correctness bug.

1. **Immutable history** is every admitted claim and document version, including invalid,
   superseded, expired, stopped, replaced, and terminal facts. History is append-only, paged, and
   queryable by stable subject, owner, generation, incarnation, time, and cursor. It never becomes
   the default list merely because it is available.
2. **Current projection** is the deterministic reduction at one store index. It selects the current
   desired head, mission generation, runtime incarnation, message lifecycle, reminder version, and
   lease interpretation. Superseded facts remain linked as history but cannot override current
   facts. Projection is replicated graph meaning; local reachability and freshness are labeled
   separately.
3. **Actionable view** is an authorization-aware, time-aware projection for one authenticated
   actor. It contains only work, attention, runtimes, messages, and actions that actor can act on
   now. It is never authority: every mutation is revalidated against exact generation,
   incarnation, revision, readiness epoch, lease, and person authority.

Default product commands and screens use the actionable view. Detail screens use current
projection and link to bounded history. `--all` or an explicit `history` command is required for
historical/stopped/superseded rows. The transition is intentionally compatibility-free: no legacy
default, fallback union, spelling alias, or environment switch restores stale rows to default
output.

## Selection rules

### Ownership and terminal state

A mission run owns its declared agents, execs, terminals, and nested runs. Ownership is a typed
edge, not indentation. A terminal remains owned by the exact run and generation that declared it.
After the owner becomes terminal and its runtime reaches `stopped`, `absent`, or `exited`, the
terminal and agent remain in history and subject detail but disappear from default actionable
lists. A new run, generation, or same-address agent does not adopt old history implicitly.

The `signal-rename-codex/sig.sup` reproduction is normative: its stopped terminal agent must remain
inspectable, but it must not appear as a live default agent, pending action, capacity consumer, or
tree node. `--all` shows it with `historical` and the terminal owner link.

### Leases and time

A claim lease is live only when `expires_at > snapshot_time`. Reads interpret expiry even before a
reconciler writes the subsequent ready claim. An expired `claimed` or `working` fact remains in
history; current projection derives `lease=expired`; actionable work exposes the step as ready with
an incremented effective readiness version and no claimant. Mutation still requires the durable
transition and exact fence, so the view cannot grant authority by itself.

Snapshot time is fixed for the whole response. Pagination preserves it. Wall-clock changes cannot
make page two disagree with page one.

### Generations, incarnations, and reminders

Only the current mission generation contributes default mission/work/attention state. Only the
current runtime incarnation contributes harness state, readiness faults, terminal control, and
session status. Old generations and incarnations remain in history with typed `superseded-by`
edges.

Attention and reminders carry a version tuple appropriate to their source:

```
(owner, generation, subject, attempt, readiness_epoch, runtime_incarnation, reminder_kind)
```

At most one unresolved reminder for the selected tuple is actionable. A newer tuple makes the old
item historical even if no close claim has been reconciled yet. Mission revision approval and
runtime-readiness attention disappear immediately when their source generation/incarnation is no
longer selected. Delivery attempts never create another logical reminder.

### Stopped agents

An agent with no selected desired state and only terminal actual observations is historical. An
agent whose selected desire is stop/retired is current but non-actionable after it is stopped.
Default agent and fleet lists exclude both. Detail-by-ID and `--all` retain desired/actual state,
owner links, stop reason, and timestamps.

### Messages, evals, and people

Person identity is always the full `person/...` subject. Eval messages addressed to
`person/eval-requester` remain person messages; they are never normalized into agent identity or
hidden because the owning eval is terminal. Unread current messages enter that person's attention.
Read or closed messages leave actionable attention and remain in history.

Only the named person (or an explicit delegated capability) may answer human gates, approve launch
or mission revisions, resolve their attention, or act on their person messages. Request bodies
cannot choose the actor. Agent, daemon, fleet, and device credentials do not imply person authority.

## Traffic classes

The daemon isolates four bounded traffic classes:

| Class | Examples | Contract |
|---|---|---|
| interactive | Now, attention, agent tree, subject detail | reserved concurrency; bounded snapshot latency |
| control | claim, approve, input, stop, pair | independent admission; idempotent and fenced |
| reconciliation | projection, runtime adoption, expiry | cannot be starved by history or replication |
| bulk | history export, replication exchange, large timelines | byte/item quotas and backpressure |

No bulk request may hold the writer, read pool, or event broadcaster needed by an interactive or
control request. Replication continues to preserve convergence, but a replication backlog cannot
turn the local control socket into an unbounded wait. Every response reports freshness and a reason
when a source is delayed, unreachable, redacted, truncated, or indeterminate.

## Screen-first read models

[`screens.json`](screens.json) is the normative inventory of human tasks and server read models.
Each model is a single versioned envelope that requires no client-side graph join. It contains
snapshot/cursor, stable deterministic ordering, bounded pages, summary counts, freshness/reasons,
typed relationships and deep links, typed actions and preconditions, and explicit empty/error
states. The required screens are:

- Now/Home sitrep;
- missions list/detail, visual graph, and live progress;
- attention inbox/detail/actions;
- launch conversation, questions, visualization, and approve-and-launch;
- machines/fleet health, capacity, projects, and work;
- agents tree/detail;
- conversations timeline/follow/send;
- activity/change-since-cursor;
- devices/pairing/scopes;
- universal subject list/detail/related/history.

The agent tree is `fleet -> mission/run -> agent role`, includes nested runs, and labels every edge
(`owns`, `member-of-generation`, `role`, `supervises`, `under`). Indentation is presentation only and
must not imply authority. Historical/stopped nodes require `--all` or history.

## CLI/API/app parity

[`cli-commands.json`](cli-commands.json) is the command-purpose inventory and installed-binary
conformance matrix. One canonical command renders each screen model for humans; `--json` returns the
same model with identical membership, fields, counts, actions, filters, and snapshot. TUI and mobile
consume the API model directly and never invoke the CLI.

Every listed command and subcommand must pass installed-binary checks for root/nested help, human
output, JSON output, valid and invalid input, empty state, composed filters, exit codes, versioned
error envelopes, documented examples, bounded bodies, and human/JSON membership/count agreement.
Unexplained empty output is a failure.

Root and nested help lead with the human job and why. Product workflows are `now`, `missions`,
`attention`, `launch`, `machines`, `agents`, `conversations`, `activity`, and `devices`. Operator
workflows are `doctor`, `repair`, `replication`, and `service`. `subject`, `claim`, `trace`, `schema`,
and `documents` are explicitly expert/debug. Internal tables and endpoints are not commands merely
because they exist. Commands with the same why are merged; legacy aliases are removed.

Two CLI reproductions are release-blocking:

- `st3 agents ls --status running --enrich` and its JSON form must select the same enriched running
  agents. Enrichment cannot run after a destructive filter that makes valid rows disappear.
- A short ID printed by `st3 conversations send` must be accepted unchanged by
  `--in-reply-to`. If the canonical value is `message/ID`, both display and parser use it; callers
  never repair an identifier manually.

Compact list, detail, human tree, and JSON are renderers of typed models, not independent queries.

## Doctor and operational repair

`st3 doctor` is read-only. Its `operational-repair` check computes the same bounded plan returned by
`GET /v1/repair` and points to `st3 repair dry-run` when contradictions exist. A plan uses
`st3.operational-repair.v0`, lists the exact affected subjects and reasons, and carries an
`orpv0:<sha256>` token over the canonically ordered repair items. Snapshot index is diagnostic and
is not part of the token.

`st3 repair apply TOKEN` applies only that exact recomputed plan. A changed plan fails with
`stale-repair-plan`; callers must inspect a new dry-run. Registered classes are terminal
descendants, orphaned readiness, expired claims, superseded attention, contradicted wake failures,
and impossible run/generation state. Apply appends normal graph transition claims and one
`repair.applied` receipt. It never deletes or rewrites claims. Repeating an accepted token returns
the replicated receipt with zero changes, and a new dry-run is clean once all listed contradictions
have converged.

## Regression fixtures

The fixtures under `crates/st3/tests/fixtures/operational-state` cover:

- the terminal `signal-rename-codex/sig.sup` runtime;
- a claimed step whose lease is already expired at snapshot time;
- stopped historical agents;
- superseded mission-revision and readiness attention;
- two versions of the same reminder;
- an eval message addressed to a person identity;
- the enriched running-agent filter and canonical reply-ID CLI regressions.
- a ready but never-claimed step expiring on wall time;
- a terminal root whose nested child run remains ready after cancellation;
- a live idle harness that accepts terminal input but does not start a turn until a full prompt is
  injected.

The passing tests validate fixture completeness and deterministic contract rules. Ignored red tests
exercise current projections and are expected to fail until behavior work lands. Implementations
must turn them green without rewriting the expected fixture outcome or removing immutable history.

The evidence, root causes, preserved-commit proof, and goal-by-goal recovery disposition are in
[`recovery-audit-2026-09-20.md`](recovery-audit-2026-09-20.md).
