# st3 KDL lifecycle

Status: current authoring and publication contract.

## One intent rule

Every KDL document starts with `version 2`. All declarations follow that line directly.

Every purpose-specific route that applies KDL is an atomic upsert. The daemon first parses, resolves document references, validates, and checks the current subject heads. It then applies every change in one transaction. One failure rejects the full operation.

Omission has no effect. Removing a declaration from a later file does not stop or delete its existing graph state. Retirement, cancellation, and refresh are explicit declarations.

The removed wrapper keyword is an error. There is no compatibility form.

`st3 missions publish FILE --as ACTOR` previews and publishes exact authored mission KDL. A person
can instead use `st3 launch` to create, review, and approve a conversationally planned mission. An
authorized agent uses `st3 work publish-mission` from the exact claimed producing step for generated
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

Before a native harness starts, st3 renders `.st3/boot.md` into its workspace. The native driver always appends this exact launch text once:

```text
Read @.st3/boot.md completely. Then list, claim, do, and finish your current st3 work.
```

A harness `prompt` is optional. An authored prompt supplies stable repository context only. Mission goals and constraints remain in the graph.

st3 refuses to replace a tracked `.st3/boot.md` with different bytes. The complete render transaction fails, and the agent does not start.

## Mission constraints

`constraint "TEXT"` can repeat on a mission or step. A step receives constraints from every ancestor mission and step, followed by its local constraints.

A constraint states a mission-specific invariant. It must not repeat universal st3 behavior or disable harness features merely to make an eval pass.

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
st3 missions start release \
  --id release/demo \
  --workspace /work/release \
  --input target=demo \
  --as person/operator
```

Use `--follow` to wait for a terminal run. Use `--print-kdl` to inspect or save the generated declaration without publishing it.

The default mission capacity is one active run. `concurrent-runs` removes the limit. `concurrent-runs max=4` sets a limit. A capacity error rejects the full publication.

An optional mission `timeout="2h"` becomes one absolute deadline on each run. It is not reset by a mission revision or daemon restart. An eval-mode run requires a timeout of 20 minutes or less.

## Waiting during claimed work

`st3 trace wait` is for a condition needed by work that the current agent has already claimed. It is not an idle-work loop.

When invoked with `--as "$ST_AGENT"`, the command watches the complete event graph. It exits early when that exact agent receives a message or becomes eligible for another ready step. It also refuses to wait when the agent has no claimed step. The explicit flag prevents an inherited environment variable from silently changing a nested shell's view. The harness can then end its turn, and native delivery can start a fresh turn for new work.

This keeps a narrow condition wait from hiding broader graph progress. Scripts outside an agent harness retain the ordinary subject-specific wait behavior.

## Durable seats

A mission without a completion block uses the finite `all-steps-exhausted` default. Declare a
long-lived conversation or worker harness as a top-level agent seat and assign finite mission work
to that exact subject.

Use `st3 agents apply`, or `st3 agents start ... --print-kdl` followed by the same command without
the preview flag. Stop a seat explicitly with `st3 agents stop`. Mission revisions change mission
work and generations without changing the seat's identity.

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

st3 expands each item after the first with a `completed` dependency on its immediate predecessor. Each step keeps its ordinary flat ID.

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

`st3 work revise RUN FILE --reason TEXT` submits the candidate through the dedicated revision route. The route publishes and applies or proposes the revision atomically.

`--print-kdl` prints only the declarative operation. It tells the operator which candidate file to publish first.

## Agent mission authority

An agent has no mission publication, start, or revision authority by default.

```kdl
mission-authority {
  publish "project/generated/*"
  start "project/generated/*"
  revise "project/generated/*"
}
```

Put this block inside the agent declaration. Use exact mission IDs or terminal `/*` namespaces without the `mission/` prefix.

An agent publishing a generated nested mission needs `publish` authority, a claimed producing step,
and an exact `produces-mission` match. Use `st3 work publish-mission` for that case.

Starting requires separate `start` authority. Revising requires separate `revise` authority and structural authority in the current generation.

Use `st3 missions publish FILE --as ACTOR` for exact authored KDL. Agent actors still need matching
authority in their current desired declaration; a candidate definition cannot grant authority to
its own publisher. Explicit person actors remain the trusted local-operator boundary.

Persons and internal system actions are unchanged. The identity check assumes a trusted local runtime because `--as` can name another actor.

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
is a successful refresh.

## Planning a new mission

Planning uses an immutable request document and a declarative launch.

```sh
st3 documents put request.md --as doc/planning/release/request
st3 launch start --id release request.md \
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

The session creates a session-scoped planner with a bounded runtime ID. Codex with no explicit model
or effort uses `gpt-6-sol` and `medium`; `st3 launch start --provider`, `--model`, and `--effort`
can select another eligible harness configuration. The daemon's `[planner]` configuration supplies
defaults for API-created launches. Each launch stores its effective planner configuration; changing
the default affects only later launches. Candidate submission is an observed result, so it is not
authored in KDL. Candidate submission creates an exact preview automatically. A blocked preview
stays durable for review.

Human approval is also observed input. It publishes the approved mission revision but does not start it. Approval and cancellation stop the session planner. Repeating either terminal action repairs a missing planner stop. The operator starts an approved new mission separately with `st3 missions start`.

`st3 launch start --print-kdl` does not store the request. It prints the required `st3 documents put` command and the planning-session KDL.

### Review or resume a launch

The launch and its document references are durable graph state. A reviewer can continue from another client after the state replicates.

```sh
st3 launch show launch/release/SESSION_ID
st3 launch preview launch/release/SESSION_ID --variant default
st3 launch revise launch/release/SESSION_ID feedback.md --as person/operator
st3 launch approve launch/release/SESSION_ID PREVIEW_TOKEN --as person/operator
```

`show` returns the current candidate and preview. `revise` stores the exact feedback document and publishes a named feedback operation. `approve` accepts only the current preview token. It cannot approve a replaced candidate.

Use `st3 launch cancel SESSION --reason TEXT --as ACTOR` to end an unwanted session. Use `--print-kdl` to inspect the cancellation declaration before publication.

## Revising a mission through planning mode

Use `--run` to bind the session to one exact run generation.

```sh
st3 launch start --run mission-run/release/demo feedback.md \
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
st3 attention ls --as person/operator
st3 attention approve step-run/RELEASE_GENERATION/deploy \
  --as person/operator \
  --reason "the exact release result is accepted"
```

`st3 attention ls --as person/operator` lists the current person-owned inbox, including pending KDL
human gates. Human authority is required and never inferred from an environment variable.

The decision target is the mission run or step run that owns the gate. The command binds the decision to the exact current request.

## Human attention

`st3 attention ls --as person/NAME` is the complete inbox for one explicit person. It includes these current items:

- pending human gates;
- launch previews that have no blockers;
- pending mission revision approvals;
- unread messages to a person;
- explicit fault attention requests.

```sh
st3 attention ls --as person/operator
st3 attention ls --as person/operator --json
```

The formatted view shows each item with its age, graph context, targets, and exact action commands. The JSON view returns the same items as structured data. The list uses oldest-first order across all item kinds.

A message leaves attention when the person reads it. Reading a sent message records delivery before the read. The person does not need to archive it. A blocked launch preview does not enter attention.

The runtime does not infer a fault request from ordinary diagnostics. A component creates one explicit request when it needs a person:

```sh
st3 attention request \
  --for person/operator \
  --title "The deployment needs recovery" \
  --reason "The automatic rollback could not restore the service." \
  --severity error \
  --target mission-run/release/demo \
  --as agent/release/operator \
  --idempotency-key release-demo-recovery
```

The idempotency key gives one stable `attention/ID` subject. A retry with the same key returns the same request.

Only the selected person can close the request. The person records whether the fault was resolved or dismissed:

```sh
st3 attention resolve attention/REQUEST_ID \
  --outcome resolved \
  --reason "The service is healthy after the manual rollback." \
  --as person/operator
```

An attention request can target any registered graph subject. It does not change that subject or its lifecycle.

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

- `st3 conversations send --print-kdl` and `st3 conversations reply --print-kdl`;
- `st3 launch start`, `revise`, and `cancel` with `--print-kdl`;
- `st3 missions start --print-kdl`;
- `st3 work revise --print-kdl`.

Repository eval fixtures are exercised by the Rust integration tests; they are not a public CLI
surface.

## Publication failure boundaries

Static failures reject the full publication. Examples include an unknown field, a missing exact document, a stale subject head, a stale generation, an immutable operation ID mismatch, a missing mission revision, or a capacity violation.

An accepted operation can cause later runtime work. A later start, stop, observer, delivery, or gate failure is a durable claim. It does not roll back the accepted intent.

This split keeps authored intent atomic and keeps real-world effects observable.

The st3 eval suite proves these workflows with isolated state and bounded run time.
