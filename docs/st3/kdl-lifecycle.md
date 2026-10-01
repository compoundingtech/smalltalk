# st KDL lifecycle

Status: current authoring and publication contract.

## One intent rule

Every KDL document starts with `version 2`. All declarations follow that line directly.

Every purpose-specific route that applies KDL is an atomic upsert. The daemon first parses, resolves document references, validates, and checks the current subject heads. It then applies every change in one transaction. One failure rejects the full operation.

Omission has no effect. Removing a declaration from a later file does not stop or delete its existing graph state. Retirement, cancellation, and refresh are explicit declarations.

The removed wrapper keyword is an error. There is no compatibility form.

`st missions publish FILE --as ACTOR` previews and publishes exact authored mission KDL. A person
can instead use `st launch` to create, review, and approve a conversationally planned mission. An
authorized agent uses `st work publish-mission` from the exact claimed producing step for generated
nested work. Every route keeps intent, authority, and provenance on a typed operation.

## Definitions do not start work

This publication creates or updates one immutable mission revision. It does not start a run.

```kdl
version 2

mission "release" state="ready" {
  goal "Publish a verified release decision."
  completion { when "all-steps-exhausted" }

  agent "builder" {
    workspace "${ST_WORKSPACE}/builder" create=#true
    harness "codex" {}
  }

  step "build" {
    assigned-to "agent/${ST_MISSION_RUN}/builder"
    goal "Build and test the release."
  }
}
```

Direct runtime declarations in a mission belong to each run of that mission. Direct declarations in a step become desired when that step activates. They stop being desired when their owner run ends or a successor generation removes them.

A native harness starts with no prompt. It takes no turn until a person types or a message is posted. When a step assigned to it becomes ready, st posts it a message that names the step. Mission goals and constraints remain in the graph.

A harness block cannot declare `prompt`. Parsing refuses it with `harness-prompt-removed`; put the instruction in a step goal or send the seat a message.

st no longer writes a `.st3` directory into a native harness workspace. It removes one that older releases wrote there, unless Git tracks something in it, and removes the `.st3/` line from the Git exclude file once no worktree sharing that file still has a `.st3` directory. A declared `render { git-exclude ".st3/" }` adds nothing.

A declared `render` operation that would change a tracked file fails that member’s complete render transaction, and the agent does not start. Other members continue reconciling; `st agents show` and `st doctor` report the fault.

## Mission constraints

`constraint "TEXT"` can repeat on a mission, a step, or an agent block inside a mission. A step receives constraints from every ancestor mission and step, followed by its local constraints, followed by the constraints of each agent it is assigned or available to. A rule written in an agent block binds only the steps that select that agent.

A constraint states a mission-specific invariant. It must not repeat universal st behavior or disable harness features merely to make an eval pass.

```kdl
mission "review" state="ready" {
  goal "Review the proposed release."
  constraint "Do not publish or deploy the release."

  step "inspect" {
    goal "Inspect the exact proposed revision."
    constraint "Do not change repository files."
  }
}
```

The work show and claim responses include the complete ordered constraint list.

## Host documents

A host can name repeatable immutable documents. Each reference must include its SHA-256 hash.

```kdl
host "build-node" {
  document "doc/hosts/build-node@0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

Publish the document bytes before this declaration. A missing version rejects the publication.

st writes no file for a host document. `st work claim` prints each document of the machine it runs on after the claimed step, under a `HOST  NODE` heading, each under its exact reference.

## Starting a mission run

A mission run names one exact mission revision. A custom run ID can be a readable operational name. The helper generates a UUIDv7 suffix when no ID is supplied.

```kdl
version 2

mission-run "release/demo" {
  mission "mission/release@0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  workspace "/work/release"
  requester "person/operator"
  input "target" "demo"
}
```

The mission and mission-run can be in one atomic publication when the run names the revision in that same file. They can also be two publications. The second form is useful when a person wants to review the mission definition before starting it.

The helper reads the current ready revision and publishes the exact run declaration:

```sh
st missions start release \
  --id release/demo \
  --workspace /work/release \
  --input target=demo \
  --as person/operator
```

Use `--follow` to wait for a terminal run. Use `--print-kdl` to inspect or save the generated declaration without publishing it.

The default mission capacity is one active run. `concurrent-runs` removes the limit. `concurrent-runs max=4` sets a limit. A capacity error rejects the full publication.

### Starting after another run

`after` makes a run wait for another mission run before any of its work starts:

```kdl
version 2

mission-run "release/deploy" {
  mission "mission/deploy@0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
  workspace "/work/deploy"
  requester "person/operator"
  after "mission-run/release/build"
}
```

`st missions start deploy --after release/build` publishes the same declaration.

st gives the run an agentless first step, `after-run`. Every other step of the normal phase depends on it. The step completes when the named run completes. It fails when that run fails or is cancelled, and the waiting run then fails.

The named run must exist when the run is published. A run cannot wait for itself. A mission with a root step named `after-run` cannot start with `after`.

A waiting run counts toward its mission's `concurrent-runs` limit. `st missions show` prints an `AFTER` line with the run it waits for and whether the wait is over. `st agents queue` names that run beside a waiting entry.

An optional mission `timeout="2h"` becomes one absolute deadline on each run. It is not reset by a mission revision or daemon restart. An eval-mode run requires a timeout of 20 minutes or less.

## Waiting during claimed work

`st trace wait` is for a condition needed by work that the current agent has already claimed. It is not an idle-work loop.

When invoked with `--as "$ST_AGENT"`, the command watches the complete event graph. It exits early when that exact agent receives a message or becomes eligible for another ready step. It also refuses to wait when the agent has no claimed step. The explicit flag prevents an inherited environment variable from silently changing a nested shell's view. The harness can then end its turn, and native delivery can start a fresh turn for new work.

This keeps a narrow condition wait from hiding broader graph progress. Scripts outside an agent harness retain the ordinary subject-specific wait behavior.

## Durable seats

A mission without a completion block uses the finite `all-steps-exhausted` default. Declare a
long-lived conversation or worker harness as a top-level agent seat and assign finite mission work
to that exact subject.

Use `st agents apply`, or `st agents start ... --print-kdl` followed by the same command without
the preview flag. Stop a seat explicitly with `st agents stop`. Mission revisions change mission
work and generations without changing the seat's identity.

A seat named `fleet/PROJECT/...` that a person declares may publish, start, and revise its
project's missions under `fleet/PROJECT/*`, so it can put person-authorized work in the graph
itself. [Agent mission authority](#agent-mission-authority) describes the default and how a
declaration narrows or withholds it.

## Ordered queue authoring

Use `queue` when source order is an intentional one-at-a-time workflow.

```kdl
queue "investigations" {
  assigned-to "agent/${ST_MISSION_RUN}/steward"
  step "measure" { goal "Measure one reported problem." }
  step "improve" { goal "Implement and verify the improvement." }
  step "ship" { goal "Ship the verified change." }
}
```

st expands each item after the first with a `completed` dependency on its immediate predecessor. Each step keeps its ordinary flat ID.

The queue ID and position appear in preview, work, graph, and generation views. Reordering a queue is a normal mission revision.

Unmoved compatible work carries into the successor generation. Moved work and its dependents restart under the normal hash rules.

## Named operations

An operational block has an ID inside its owner subject.

```kdl
version 2

mission-run "release/demo" {
  cancellation "operator-stop" {
    reason "the release was withdrawn"
  }
}
```

The identity is the owner subject, operation kind, and operation ID. Repeating the exact content is a no-op. Reusing that ID with changed content is an error. A retry with new intent needs a new operation ID.

Failed static validation does not record the operation. The caller can correct the document and publish it with a new ID. An accepted asynchronous action can later record a durable failure claim.

### Revision

Publish the candidate mission revision first. Then publish an operation against the run's exact current generation.

```kdl
version 2

mission-run "release/demo" {
  revision "add-security-gate" {
    mission "mission/release@fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
    from "run-generation/01990000000070008000000000000000"
    reason "the build exposed a new security boundary"
  }
}
```

An authorized revision with immediate cutover creates one successor generation atomically. Compatible completed work moves forward. Changed or dependent work becomes available again. The old generation becomes superseded.

A human-protected revision creates a durable revision proposal. The named operation remains accepted and idempotent. A reviewer approves the exact preview through the observed review command. The approval then creates the successor generation.

`st work revise RUN FILE --reason TEXT` submits the candidate through the dedicated revision route. The route publishes and applies or proposes the revision atomically.

`--print-kdl` prints only the declarative operation. It tells the operator which candidate file to publish first.

## Agent mission authority

A top-level seat named `fleet/PROJECT` or `fleet/PROJECT/...` that a person declared may publish,
start, and revise missions under `fleet/PROJECT/*` by default. The seat
`fleet/website/standing/website` may publish `fleet/website/refresh` and start and revise its runs,
and no mission of another project. `st agents show` prints the authority and its source:

```text
AUTHORITY    publish, start, revise mission/fleet/website/* (default)
```

No other agent holds mission authority by default. An agent declared inside a mission stays bounded
by that mission, even when its run ID puts it under `fleet/PROJECT/`. A seat whose current
declaration an agent wrote with `seat-authority` holds nothing by default, so an agent never lends
a seat authority it lacks; the seat regains the default when a person declares it again. st records
the writer on each declaration; a declaration from before st recorded writers counts as a person's.

A `mission-authority` block in the declaration replaces the default:

```kdl
mission-authority {
  publish "project/generated/*"
  start "project/generated/*"
  revise "project/generated/*"
  cancel "project/generated/*"
}
```

Use exact mission IDs or terminal `/*` namespaces without the `mission/` prefix. Name narrower
rules than the default, such as `publish "fleet/website/docs/*"`, or withhold all mission authority
with `mission-authority "none"`. Cancellation needs an explicit `cancel` rule; the project default
continues to grant only `publish`, `start`, and `revise`. Use `st missions cancel RUN --as agent/PATH
--reason TEXT` to cancel a run of a mission under a granted path. The daemon checks the current
grant and the run generation before cancellation.

An agent publishing a generated nested mission needs `publish` authority, a claimed producing step,
and an exact `produces-mission` match. Use `st work publish-mission` for that case.

Starting requires separate `start` authority. Revising requires separate `revise` authority and structural authority in the current generation.

Use `st missions publish FILE --as ACTOR` for exact authored KDL. Agent actors still need matching
authority in their current desired declaration; a candidate definition cannot grant authority to
its own publisher. Explicit person actors remain the trusted local-operator boundary.

Mission starts require an explicit `--as`; the placeholder `person/requester` is not a valid run
requester. A harness with `ST_AGENT` can mutate only as its own seat. On Linux, the local Unix API
also binds a connection from a harness process or its descendants to that seat and refuses a
different actor in mutation requests. Persons and internal system actions remain on the trusted
local runtime boundary.

### Runtime reset

```kdl
version 2

mission-run "release/demo" {
  reset "retry-builder" {
    runtime "agent/builder"
    from "run-generation/01990000000070008000000000000000"
    reason "the operator corrected the runtime dependency"
  }
}
```

The daemon expands a run-local runtime ID inside the owner run. The generation fence prevents an old reset from affecting a replacement generation.

### Resource refresh

```kdl
version 2

resource "release-pr" {
  refresh "after-push" {
    timeout "30s"
  }
}
```

The resource must already have an active observer. Refresh is a declarative mission operation; the
current public CLI does not expose a standalone resource-refresh shortcut. An unchanged observation
is a successful refresh. A refresh always asks the provider again, even inside the window in which
a GitHub observer reuses its last responses; GitHub answers an unchanged conditional request
without spending rate limit.

## Agent queue authority

An agent cannot reorder a seat's queue by default. A person grants that in the agent's declaration:

```kdl
queue-authority {
  move "fleet/fabric/builder"
  move "fleet/review/*"
}
```

Each rule names an exact seat identity or a terminal `/*` namespace, without the `agent/` prefix.
The agent then moves runs with `st agents queue move SEAT RUN --top --as agent/PATH`. The move is
recorded with the agent as its actor. The daemon reads the grant from the agent's current desired
declaration when the move arrives, as it does for mission authority. [Agent seat
queues](seat-queue.md) describes the queue and the move.

## Agent declaration authority

A person can authorize a seat to apply, start, and stop agent declarations under named paths:

```kdl
agent-authority {
  apply "fleet/example/operations/*"
  apply "fleet/example/ci/*"
}
```

Each `apply` rule names an exact agent identity or a terminal `/*` namespace without `agent/`.
The seat uses its own identity with `st agents apply FILE --as agent/PATH`, `st agents start`,
and `st agents stop`. Existing `seat-authority { declare PATH; stop PATH }` rules still work.

An agent with `agent-authority` may also delegate authority in a top-level declaration it applies.
Every proposed rule must have the same authority kind and verb as a rule the caller currently
holds, over the same or a narrower path. This covers `mission-authority`, `queue-authority`,
`seat-authority`, and `agent-authority` itself. An exact rule cannot delegate a namespace, and
`project/*` does not cover `project` or `project-other/*`. A wider path, extra verb, or different
kind is refused with `agent-authority-grant-denied`, which names the refused rule. The candidate
cannot grant itself permission: checks read the caller's current declaration before applying it.
Delegated seats have no implicit project mission authority; their explicit rules are the grant.

Mission publication and revision retain their existing guard: authority in agents declared
inside a mission, step, or nested mission must match authority already published for that agent.
This includes `agent-authority`; publishing a mission cannot bypass declaration grants.

## Planning a new mission

Planning uses an immutable request document and a declarative launch.

```sh
st documents put request.md --as doc/planning/release/request
st launch start --id release request.md \
  --workspace /work/release \
  --as person/operator
```

The helper stores the request first. It then publishes a declaration like this:

```kdl
version 2

planning-session "planning/release/01990000000070008000000000000000" {
  mission "release"
  request "doc/planning/release/request@REQUEST_SHA256"
  workspace "/work/release"
  requester "person/operator"
  planner "codex" {}
}
```

The session creates a session-scoped planner with a bounded runtime ID. The planner starts idle;
its instructions arrive as a message titled "Launch request". Codex with no explicit model
or effort uses `gpt-6-sol` and `medium`; `st launch start --provider`, `--model`, and `--effort`
can select another eligible harness configuration. The daemon's `[planner]` configuration supplies
defaults for API-created launches. Each launch stores its effective planner configuration; changing
the default affects only later launches. Candidate submission is an observed result, so it is not
authored in KDL. Candidate submission creates an exact preview automatically. A blocked preview
stays durable for review.

Human approval is also observed input. It publishes the approved mission revision but does not start it. Approval and cancellation stop the session planner. Repeating either terminal action repairs a missing planner stop. The operator starts an approved new mission separately with `st missions start`.

`st launch start --print-kdl` does not store the request. It prints the required `st documents put` command and the planning-session KDL.

### Review or resume a launch

The launch and its document references are durable graph state. A reviewer can continue from another client after the state replicates.

```sh
st launch show launch/release/SESSION_ID
st launch preview launch/release/SESSION_ID --variant default
st launch revise launch/release/SESSION_ID feedback.md --as person/operator
st launch approve launch/release/SESSION_ID PREVIEW_TOKEN --as person/operator
```

`show` returns the current candidate and preview. `revise` stores the exact feedback document and publishes a named feedback operation. `approve` accepts only the current preview token. It cannot approve a replaced candidate.

Use `st launch cancel SESSION --reason TEXT --as ACTOR` to end an unwanted session. Use `--print-kdl` to inspect the cancellation declaration before publication.

## Revising a mission through planning mode

Use `--run` to bind the session to one exact run generation.

```sh
st launch start --run mission-run/release/demo feedback.md \
  --workspace /work/release \
  --as person/operator
```

The declaration contains both `target-run` and `target-generation`. If the run moves before publication, the launch is rejected as stale.

Feedback reopens an existing session with a named operation:

```kdl
version 2

planning-session "planning/release/01990000000070008000000000000000" {
  feedback "clarify-security-gate" {
    document "doc/planning/release/feedback@FEEDBACK_SHA256"
    variant "default"
  }
}
```

The exact feedback document is stored first. The session returns to the planner and replaces the prior preview.

Approval of a targeted launch preview publishes the candidate mission and creates its revision proposal. When the same person is the required revision reviewer, that one approval counts at both boundaries.

## Human gates

A human gate is declared with the mission. Its decision is observed input, not authored intent.

```kdl
gate "the operator approves deployment" type="human" {
  reviewer "person/operator"
  question "Deploy this exact result?"
  review "resource/release-decision"
}
```

The mission pauses at the gate. A later review command records the decision against the exact gate request. Editing and republishing the mission does not forge a decision.

```sh
st attention ls --as person/operator
st attention approve step-run/RELEASE_GENERATION/deploy \
  --as person/operator \
  --reason "the exact release result is accepted"
```

`st attention ls --as person/operator` lists the current person-owned inbox, including pending KDL
human gates. Human authority is required and never inferred from an environment variable.

The decision target is the mission run or step run that owns the gate. The command binds the decision to the exact current request.

Human gates default to `mode="approve"`: `st attention approve` passes the gate and
`st attention reject --reason TEXT` fails it with the reviewer's reason. A worker-owned step
can instead use `mode="feedback"`:

```kdl
gate "review the draft" type="human" mode="feedback" {
  reviewer "person/operator"
  question "Does the draft need changes?"
}
```

That review offers approve and request changes. Run `st attention request-changes
step-run/GENERATION/draft --reason "Add the missing source." --as person/operator` to send
feedback to the step's worker. The step starts a new attempt without entering `failed`, and its
goals include `Reviewer feedback: Add the missing source.` The worker must claim and complete that
new attempt; the next review is a distinct request. Feedback gates require a worker-owned step.
Mission-level, loop, and agentless gates use approve mode.

## Human attention

`st attention ls --as person/NAME` reads current source state: ready person steps, human
gates, valid launch previews, outstanding proposal reviews, unread person messages/reminders,
and failures whose current source needs a person. Priority precedes waiting age. Each card's
identity includes its source, recipient and waiting episode. A cancelled or retired owner,
removed source, replacement generation or changed attempt removes the card before cleanup.
Failed sources can retain their own repair fault. Held subscription requests do not appear.

```sh
st attention ls --as person/operator
st work ask --for person/operator --title "Choose a release date" \
  --reason "The release needs a date" --step step-run/release/prepare \
  --as agent/release/operator --idempotency-key release-date
st work done step-run/release/ask-ID --as person/operator --summary "Friday"
```

An ask creates a person-assigned runtime step in the live owning generation. Its origin waits in
`waiting-person`, without a lease or time/retry consumption. Only the assigned person can
complete it. The response resumes the same origin attempt with a new readiness epoch. A requester
can use `st work cancel-ask STEP --as AGENT --summary TEXT`. An unclaimed live requester can
use `--new-run NAME`; claimed work must name `--step`. Repeated keys return the same source.

A person message leaves on read or archive, without an age expiry. A launch needs its current
valid preview. Faults describe source recovery and inspection; they have no independent dismiss
state. Legacy `attention request`, `resolve` and `withdraw` return `attention-migrated`.
Historical attention claims remain audit data; only provably live agent asks are imported as
person steps. `st doctor` reads the same snapshot for attention age checks.

## Cancellation and cleanup

Cancellation is explicit and additive. Omission never cancels a run.

```kdl
version 2

mission-run "release/demo" {
  cancellation "withdraw-release" {
    reason "the requester withdrew the release"
  }
}
```

Cancellation revokes active work claims, enters adjacent `finally` work, and cascades to descendant mission runs. The run becomes terminal only after its owned runtimes stop.

A launch uses the same noun:

```kdl
version 2

planning-session "planning/release/01990000000070008000000000000000" {
  cancellation "operator-stop" {
    reason "planning is no longer needed"
  }
}
```

## Print-only helpers

Intent helpers support `--print-kdl`. Print-only mode performs no publication and no document upload.

The current helpers are:

- `st conversations send --print-kdl` and `st conversations reply --print-kdl`;
- `st launch start`, `revise`, and `cancel` with `--print-kdl`;
- `st missions start --print-kdl`;
- `st work revise --print-kdl`.

Repository eval fixtures are exercised by the Rust integration tests; they are not a public CLI
surface.

## Publication failure boundaries

Static failures reject the full publication. Examples include an unknown field, a missing exact document, a stale subject head, a stale generation, an immutable operation ID mismatch, a missing mission revision, or a capacity violation.

An accepted operation can cause later runtime work. A later start, stop, observer, delivery, or gate failure is a durable claim. It does not roll back the accepted intent.

This split keeps authored intent atomic and keeps real-world effects observable.

The st eval suite proves these workflows with isolated state and bounded run time.
