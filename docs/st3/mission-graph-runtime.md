# st mission graph runtime

Status: current language and runtime specification.

See [kdl-lifecycle.md](./kdl-lifecycle.md) for complete day-to-day publication workflows.

Every st KDL document starts with `version 2`. Declarations follow the version directly.

A mission is an immutable definition in the claims graph. Publishing a mission does not start it.

A mission run has one stable subject. Each immutable run generation binds that run to one exact mission revision.

## Core rules

- A mission has `draft`, `ready`, or `retired` state.
- Only a ready mission can start.
- A mission needs one through three `goal` nodes.
- A step accepts zero through three `goal` nodes.
- Missions and steps can repeat `baseline` and `gate`.
- Missions and steps can contain one `produces` block.
- All sibling products must exist. All sibling gates must pass.
- A mission defaults to `completion { when "all-steps-exhausted" }`.
- Durable availability belongs to top-level agent seats, not open mission runs.
- `depends-on` defines execution order. Source order defines display order only.
- A missing `depends-on` makes a step a root. It does not imply a dependency on the previous step.
- st rejects missing step references and dependency cycles.
- st does not accept `outcome`, `judges`, or `judge`.
- `assigned-to` and `available-to` can set a mission default or select one step.
- `fresh-context` on a step starts its claiming seat in a new harness session for that step. On an agent seat, it does so for every step the seat claims.
- `agentless` is step-only. A step with no inherited selector is also agentless.
- A selector does not grant revision authority.
- A mission allows one active run by default.
- `concurrent-runs` enables concurrent active runs. An optional `max` property bounds them.
- A mission can declare exact text and resource inputs.
- A mission can declare one absolute run `timeout`.
- `terminal "NAME" { ... }` is the canonical interactive runtime declaration. The legacy spelling `pty "NAME" { ... }` is temporarily accepted with a preview warning and will be removed before the friend-ready v0.
- An eval entry mission must declare a timeout no greater than 20 minutes.
- A first-class `loop` runs one bounded child mission for each round.
- A loop always declares `max-rounds`. It can also declare one total `timeout`.

## Complete example

```kdl
version 2

mission "release" state="ready" revisions="human-only" revision-reviewer="person/alex" revision-cutover="when-idle" {
    input "source" kind="resource"
    goal "Produce a verified release decision."
    goal "Keep the source and test evidence visible in the graph."
    completion { when "all-steps-exhausted" }

    agent "release.lead" {
        workspace "${ST_WORKSPACE}/lead"
        harness "codex" {
          model "gpt-5.6-sol"
          effort "medium"
        }
    }
    agent "release.test" {
        under "release.lead" reason="the lead combines the test evidence"
        workspace "${ST_WORKSPACE}/test"
        harness "codex" {
          model "gpt-5.6-sol"
          effort "medium"
        }
    }

    baseline "the release request is ready" {
      field "status" "${input.source}" "is" "ready"
    }

    produces {
      resource "mission-run/${ST_MISSION_RUN}/release-decision" {
        kind "custom.st3.release-decision"
        state "published"
      }
    }

    gate "the requester approves the release" type="human" {
      reviewer "person/alex"
      question "Is this release ready?"
      review "resource/mission-run/${ST_MISSION_RUN}/release-decision"
    }

    step "start-team" {
      agentless
      title "The release team is ready"
      gate "the lead exists" { exists "agent/${ST_MISSION_RUN}/release.lead" }
      gate "the test agent exists" { exists "agent/${ST_MISSION_RUN}/release.test" }
    }

    step "inspect" timeout="20m" {
      title "The source is inspected"
      goal "Inspect the exact release source and publish an inspection report."
      assigned-to "agent/${ST_MISSION_RUN}/release.lead"
      depends-on { step "start-team" completed }
      produces {
        resource "mission-run/${ST_MISSION_RUN}/inspection" {
          kind "custom.st3.release-inspection"
          state "published"
        }
      }
      gate "the report contains a revision" {
        field "revision" "resource/mission-run/${ST_MISSION_RUN}/inspection" "starts-with" "git:"
      }
    }

    step "verify" timeout="20m" {
      title "The release decision is verified"
      goal "Run the tests and publish the final release decision."
      assigned-to "agent/${ST_MISSION_RUN}/release.test"
      depends-on { step "inspect" completed }
      retry { attempts 2; backoff "30s" }
      gate "the release tests pass" {
        exec "./verify-release.sh"
        host "local"
        workspace "${ST_WORKSPACE}/test"
        time-limit "5m"
      }
    }
}
```

The mission body owns the complete mission revision. Its agents use stable subjects inside one mission run.

The mission baseline protects the run admission boundary. The inspection product is intermediate step output.

The release decision is a final mission product. The human gate is a mission-level acceptance condition.

A human gate uses `mode="approve"` by default. On a worker-owned step, `mode="feedback"`
offers `approved` and `changes-requested` instead of approve and reject. A request for changes
requires free text, sends it to the worker, and starts a new attempt with that text appended to
the step goals. See [the feedback example](../../examples/st3/human-feedback.kdl).

After acceptance, cleanup stops both agents before the run becomes completed.

## Mission syntax

```kdl
mission "MISSION_ID"
  state="ready"
  timeout="2h"
  revisions="human-only"
  revision-reviewer="person/reviewer"
  revision-cutover="when-idle" {
  goal "One measurable mission goal."
  goal "An optional second goal."
  goal "An optional third goal."
  constraint "A mission-specific rule for this work."

  input "message" kind="text"
  input "source" kind="resource"
  concurrent-runs max=4

  assigned-to "agent/${ST_MISSION_RUN}/owner"
  // Or repeat available-to. A mission cannot declare agentless.

  completion { when "all-steps-exhausted" }
  // Or: completion { depends-on { step "publish" completed } }

  baseline "NAME" { GRAPH_PREDICATE }
  produces { PRODUCT... }
  gate "NAME" { GATE_BODY }

  MISSION_AGENTS...

  step "STEP_ID" { ... }
  finally { step "CLEANUP_ID" { ... } }
}
```

The `state` property is required for a top-level mission. A nested mission defaults to ready because it is already part of a submitted parent revision.

Mission IDs can contain path separators. Step IDs cannot. IDs cannot be empty, contain whitespace, start or end with `/`, or contain `//`.

Mission goal order is preserved. Each mission must have one, two, or three goals.

A mission can repeat constraints, baselines, and gates. Gate and baseline names must be unique within that mission. A mission has at most one `produces` block.

A mission can repeat `input`. Each input name is unique and uses `kind="text"` or `kind="resource"`.

The input set and kinds cannot change across revisions of an active run. Input values remain immutable across all run generations.

The default active run limit is one. Bare `concurrent-runs` removes the limit. `concurrent-runs max=4` sets a positive limit.

When active revisions declare different limits, st uses the strictest limit. A lower limit does not cancel existing runs.

An exact idempotent retry returns its existing run before the capacity check. A direct start error lists the active run subjects.

`revisions="human-only"` is optional and inherited by child steps. `revision-reviewer` requires that protection.

The reviewer defaults to the mission run requester. `revision-cutover` is `restart-active` by default or `when-idle` when declared. The value in the current generation controls how its successor starts; a candidate cannot select its own cutover.

A mission can contain direct declarations. Direct agents in the mission can revise the complete mission.

A mission can contain zero steps. Missions default to `completion { when "all-steps-exhausted" }`,
so a zero-step mission completes immediately.

`timeout` is optional for an ordinary mission. It starts when the mission run is created and applies to the complete run, not one step or generation. A revision cannot extend or reset the stored deadline.

The daemon arms an exact wake for the nearest deadline. It does not depend on a client process or periodic polling. At expiry, st fails the run, terminates descendant runs, skips remaining normal and final work, removes run-owned runtime state, and records the timeout as the eval failure reason when the run is an eval.

Every eval entry mission needs a timeout of 20 minutes or less. The store enforces this rule for both the eval API and a directly published eval-mode mission run.

`completion` accepts one shortcut or one dependency block. The two forms cannot appear together.

`when "all-steps-exhausted"` selects all normal steps without listing them. Failed retryable work is not exhausted.

The dependency form uses the same explicit dependency language as a step. It can select a smaller completion frontier.

A completion dependency cannot reference a final step.

Omitting `completion` means `all-steps-exhausted`. Long-lived harness availability is modeled by a
top-level agent seat, not by omission on a mission.

A mission that should no longer start leaves every list when it is retired:

```sh
st missions retire mission/MISSION_ID --as person/operator
```

Retirement publishes the current definition again with `state="retired"`, naming who retired it.
The mission leaves `st missions ls`, `st missions tree` and every client, and cannot start. Its
revisions and runs stay in its history (`st missions ls --all`, `st missions show`). A person, or an
agent with `publish` authority for the mission, can retire it once no run of it is active. Publishing
a ready revision brings it back.

## Run ownership and concurrency

A mission run owns the execution state declared inside that run. A top-level `agent` is instead a
first-class durable seat with no mission owner. Seats can receive messages and claim work from many
finite missions over their lifetime.

The origin of the `mission-run.created` claim advances the mission run. It also materializes the run declarations.

A replica stores the run and its steps. It can accept eligible work claims, but it does not evaluate the run.

This rule prevents two nodes from creating different local runtimes for one replicated run. An explicit member `host` can place work elsewhere.

An authored runtime ID is local to the run. st expands it to these subjects:

- `agent/RUN/LOCAL_ID`;
- `exec/RUN/LOCAL_ID`;
- `pty/RUN/LOCAL_ID`;
- `observer/RUN/LOCAL_ID`;
- `subscription/RUN/LOCAL_ID`;
- `schedule/RUN/LOCAL_ID`.

Two concurrent runs can use the same local IDs. Two declaration sites in one generation cannot declare the same runtime subject.

Run-owned runtimes remain scoped to the run and stop during terminal cleanup. A root `stop` can stop
an exact top-level agent seat. Named mission-run cancellation remains the way to stop a run and all
of its run-owned state.

The default mission permits one nonterminal run. This default also lets `st missions show MISSION` identify the current run.

Bare `concurrent-runs` permits unlimited nonterminal runs. `concurrent-runs max=N` sets a positive limit.

The capacity check runs after the idempotency check. A child start waits when capacity is full, but a direct start returns `mission-run-capacity`.

Missions and agents do not support in-place ownership changes. Publish a replacement mission and cancel the old run when ownership must change.

## Step syntax

```kdl
step "STEP_ID" timeout="20m" revisions="human-only" revision-reviewer="person/reviewer" {
  title "A display title"
  goal "One optional goal."
  goal "A second optional goal."
  goal "A third optional goal."
  constraint "A step-specific rule for this work."
  available-to "agent/${ST_MISSION_RUN}/worker-a"
  available-to "agent/${ST_MISSION_RUN}/worker-b"
  fresh-context
  document "doc/project/request@SHA256"

  depends-on {
    step "earlier-step" completed
  }

  baseline "NAME" { GRAPH_PREDICATE }
  DESIRED_STATE...
  mission "nested-work" { ... }
  retry { attempts 3; backoff "30s" }
  produces { PRODUCT... }
  produces-mission "generated-mission"
  uses-mission output-of="producer-step"
  after-run "mission-run/RUN_ID"
  gate "NAME" { GATE_BODY }
}
```

`title`, `assigned-to`, `agentless`, `fresh-context`, `mission`, `retry`, `produces`, `produces-mission`, `uses-mission`, and `after-run` are single fields.

`fresh-context` is a bare node. Before the daemon wakes a seat for that step, it stops the old harness session and starts a new, idle one. The step's work message arrives in the new session. A claim from the old incarnation is refused while the reset is pending. Steps without this option keep the current session unless the seat opts in for every step. A retry is a new attempt and receives its own fresh session.

A retry repeats one failed step attempt. It handles a bounded transient failure.

`available-to`, `goal`, `constraint`, `document`, `depends-on`, `baseline`, and `gate` can repeat. A step accepts at most three goals.

`timeout` is an execution budget for one step attempt. It advances only while a worker holds a live claim in `claimed` or `working`; dependency waits, assignment waits, blocked gates, ready time, verification, and time after lease expiry do not consume it. Release and reclaim resume the same attempt's accumulated budget, while retry starts a new attempt with a fresh budget. Work projections expose the active interval start, accumulated execution milliseconds, and configured timeout so an operator can explain an expiry after restart or replication. A step cannot use a deadline gate because its timeout is its worker-execution budget.

If a step produces a native harness driver, the step waits for a ready, working, or idle harness observation from the current runtime incarnation. An observation with another incarnation cannot satisfy the step. An old observation without an incarnation applies only when it was recorded after the current runtime epoch began.

A driver declared with `restart "never"` that exits, vanishes, or fails to start before readiness fails the step immediately. A restartable driver remains pending while its restart policy can still recover it. It fails when that policy raises an unrecoverable decision. Driver readiness is lifecycle state and does not consume the step's claimed-execution budget.

Terminal ownership is recursive. When a root mission run becomes completed, failed, or cancelled, every nested run is terminalized and every nonterminal descendant step is cancelled. Repeating the terminal transition repairs any orphaned descendant left by an interrupted older daemon; once the tree is clean, the same operation is an explicit no-op. Current work queries and work actions also fence on the root owner, so stale readiness or replicated work claims cannot reopen a terminal tree. Only an explicit step retry or run revision reopens a failed run, and it does so in a new generation. `work ls --all` retains the history with the exact owner run and non-actionable reason.

The daemon gives a running native harness 60 seconds to become ready. At the deadline, the daemon preserves the PTY and records `runtime.readiness-deadline-reached`. It requests attention from `person/operator` once. It does not restart the runtime or send input. A later ready observation from the same incarnation resolves that attention item as `daemon/runtime`.

`finally {}` contains final-phase steps. Final steps run after normal success, failure, or cancellation.

A mission can have one `finally` block. Final steps can depend on other final steps.

Dependencies cannot cross the normal and final phases. A final step does not make normal work optional.

Step revision protection adds to inherited mission protection. Direct agents in the step can revise that step subtree.

## Ordered queues

A queue is concise syntax for a strict sequence of ordinary steps.

```kdl
queue "investigations" {
  assigned-to "agent/${ST_MISSION_RUN}/steward"
  step "measure" { goal "Measure the current behavior." }
  step "improve" { goal "Implement and verify one improvement." }
  step "ship" { goal "Ship the verified improvement." }
}
```

The queue needs one or more steps. Queue steps keep flat mission step paths.

Each item after the first depends on its immediate predecessor being `completed`. Explicit additional dependencies remain valid.

A queue can set exactly one selector family. A step selector overrides the queue selector, and a queue selector overrides the mission selector.

The parsed mission and runtime views retain the queue ID and one-based position. Text and JSON views expose both values.

An ordinary mission body can contain multiple queues and ordinary steps. A nested mission can also contain a queue.

A queue cannot contain another queue or a `finally` block in this version. A `finally` block cannot contain a queue.

Queue order is definition state. Reordering items changes moved step hashes and uses the ordinary successor-generation compatibility rules.

## Bounded loops

A loop is a mission graph node. It is not a step property.

```kdl
loop "improve" timeout="2h" {
  max-rounds 6

  metric "quality" direction="higher" min-improvement=0.01 {
    field "score" "resource/result"
  }

  stop {
    plateau metric="quality" rounds=2
    repeated-failure 2
    token-budget 50000
  }

  until {
    gate "the result is ready" {
      field "state" "resource/result" "is" "ready"
    }
  }

  round {
    completion { when "all-steps-exhausted" }
    step "work" {
      assigned-to "agent/${ST_ROOT_MISSION_RUN_ID}/worker"
      goal "Improve the result for round ${loop.round}."
    }
  }

  keep-if metric="quality"
  on-keep {
    completion { when "all-steps-exhausted" }
    step "keep" {
      agentless
      exec "keep-result" {
        host "local"
        workspace "${ST_WORKSPACE}"
        command "./keep.sh"
        restart "never"
      }
      gate "the keep action passes" {
        field "exit_code" "exec/${ST_MISSION_RUN}/keep-result" "is" 0
      }
    }
  }
  on-discard {
    completion { when "all-steps-exhausted" }
    step "discard" {
      agentless
      exec "discard-result" {
        host "local"
        workspace "${ST_WORKSPACE}"
        command "./discard.sh"
        restart "never"
      }
      gate "the discard action passes" {
        field "exit_code" "exec/${ST_MISSION_RUN}/discard-result" "is" 0
      }
    }
  }

  on-exhausted { fail }
}
```

`max-rounds` is required. Its value is between 1 and 100.

The optional loop `timeout` covers all rounds, metrics, gates, and branch work. A loop uses the first bound that it reaches.

The `round` block is one embedded mission. It needs an explicit `completion` block.

Generated round and branch missions inherit the smaller loop or parent mission timeout.

The embedded mission revision is immutable. A client cannot start it directly. The parent loop starts each exact child run.

An agent declared inside `round` belongs to that child run. st removes it when the child run ends.

An agent declared outside the loop belongs to the root run. A round can address it with `ST_ROOT_MISSION_RUN_ID`.

`until` contains repeated gates. All exit gates must pass.

Each round writes one `loop.round-result` claim. The result records the child run, metrics, token use, status, and feedback document.

The feedback document is immutable. The next round receives its exact reference through `${loop.feedback}`.

A metric has `direction="higher"` or `direction="lower"`. Its value must be a finite number.

A metric can read a numeric resource field, run an exec command, or normalize one exit gate to zero or one.

An exec metric must print one finite number. Its `time-limit` defaults to two minutes.

`keep-if` compares one metric with the best kept result. Its `min-improvement` value defines a meaningful improvement.

`on-keep` and `on-discard` are ordinary embedded missions. They make file, Git, or other projection behavior explicit.

The loop has no built-in Git behavior. A failed keep or discard mission stops the loop immediately.

`stop` can limit a plateau, repeated round failures, and total structured token use. A reached stop rule exhausts the loop.

`on-exhausted` defaults to `fail`. It can contain `succeed` or one human gate instead.

Every loop that stops short raises one attention item: a loop that fails at exhaustion or in its
human review, and a loop that a cancelled round or a failed keep or discard branch stops. The item
targets the loop and its run. It names the cause and the command that continues or ends the loop:
`st work retry STEP` for a failed step, which runs the next round, or `st missions cancel RUN` and
`st missions start MISSION` for a cancelled one. It asks the person who requested the run, or
`person/operator` when an agent requested it. Each stop raises its own item, and an item closes
when the loop runs again, a revision replaces its generation, or its run is cancelled.

A failed loop can name the item's title, reviewer, and severity:

```kdl
on-exhausted {
  fail
  attention "Automatic review failed" {
    reviewer "person/alex"
    severity "error"
  }
}
```

The severity can be `warning` or `error`. An attention block is not valid after `succeed` or a human gate.

A human exhaustion approval accepts the current best result. More rounds require a published mission revision.

A cancelled round or a structural child failure stops immediately. An ordinary failed round can start the next round.

### Collection and candidate composition

Loops are deliberately sequential: one bounded `round` mission runs at a time until `until`
passes or `max-rounds` is exhausted. The former `for-each`, `max-parallel`, and `candidates`
loop forms are not part of the grammar.

Model a known collection as explicit mission steps or a queue. Model best-of-N work as explicit
candidate steps followed by a gated selection step. This keeps concurrency, ownership, retry, and
review visible in the ordinary mission graph instead of hiding a second scheduler inside `loop`.

## Goals

A goal is a concise, falsifiable statement about the result.

Use one `goal` node for one statement. Use up to three nodes when the mission or step has separate required outcomes.

Do not use source order or bullet syntax inside one string to create hidden execution structure. Steps and `depends-on` own execution structure.

## Mission constraints

A constraint states a rule that is specific to one mission, step, or agent.

A mission, a step, and an agent block inside a mission can repeat `constraint`. An exact duplicate in one block is an error.

A mission or step constraint binds every step inside it, whichever agent does the work. Write a rule for one agent in that agent's block:

```kdl
agent "builder" {
  workspace "${ST_WORKSPACE}/build"
  harness "claude" { model "claude-sonnet-5" }
  constraint "Do not push the release branch."
}

step "build" {
  assigned-to "agent/${ST_MISSION_RUN}/builder"
}

step "merge" {
  assigned-to "agent/${ST_MISSION_RUN}/merger"
  depends-on { step "build" completed }
}
```

The builder's constraint binds `build` and not `merge`.

An agent constraint applies to every step in the run whose selector names that agent: `assigned-to` the agent, or an `available-to` pool that includes it, because any agent in the pool can claim the step. It never applies to an agentless step.

Name the agent by the subject its block declares, `agent/${ST_MISSION_RUN}/NAME`. Preview warns when no step selects an agent that has constraints.

The effective order is each outer mission, its parent step, each nested mission, the leaf step, and then each selected agent. An agent constraint that repeats an earlier entry appears once.

An agent constraint is a work rule, not runtime configuration. It is not part of the agent's runtime declaration. Changing it revises the steps that select the agent, as a step constraint does.

A top-level agent seat has no mission steps, so it cannot declare a constraint. Publication fails with `agent-constraint-outside-mission`.

st shows the effective list when an agent shows or claims work.

Do not repeat universal st behavior as a mission constraint. The skill that `st skill` prints describes how an agent uses st.

Do not disable harness features to make an eval pass. A mission constraint must describe a real mission requirement.

## Mission inputs

A ready top-level mission can declare text and resource inputs.

```kdl
input "message" kind="text"
input "source" kind="resource"
```

A start request must provide exactly the declared names. Missing and extra names are errors.

Use `${input.message}` and `${input.source}` in execution content. st preserves quoted and multiline text when it writes interpolated KDL.

A resource input accepts `resource/NAME` or `resource/NAME@CLAIM_ID`.
st resolves a bare subject to its latest `resource.observed` claim atomically.

The run stores the exact resource subject and claim ID. Later claims do not change gates, inspection, or execution for that input.

Inputs do not support defaults, lists, secrets, schemas, or automatic environment export. Put an input in `env` when a process needs it.

Nested child missions cannot declare inputs in this version.

```sh
# After the mission has been approved through st launch:
st missions start MISSION_ID \
  --input message="Review this release." \
  --input source=resource/release-source \
  --as person/operator

st claim resource/mission-inputs/source resource.observed \
  --field kind=custom.st3.document-source \
  --field state=ready
```

## Baselines

A baseline records state that must be true before new work starts.

```kdl
baseline "the incident is still open" {
  field "status" "resource/incident" "is" "open"
  lacks "doc/incident/decision@SHA256" "closed"
}
```

A baseline contains one or more graph predicates. Its predicates form an AND relation.

Baselines accept `exists`, `empty`, `field`, `has`, and `lacks`. They do not execute shell, LLM, human, or deadline work.

Mission baselines run before root work admission. st does not materialize a mission runtime before these baselines pass.

A false mission baseline puts the mission run in blocked state. st rechecks it after relevant graph changes while admission remains blocked.

Once normal work is admitted, the mission baseline is latched. st does not re-evaluate it as a continuous gate.

Step baselines run after dependencies hold and before each attempt becomes ready. A false step baseline blocks the step. It does not consume an attempt. A retry checks the baseline again.

A baseline is not historical storage by itself. The mission request or a prior claim must publish the measured state that the predicate names.

## Products

`produces` declares graph state that the work promises to create.

```kdl
produces {
  resource "mission-run/${ST_MISSION_RUN}/artifact" {
    kind "custom.st3.build-artifact"
    state "published"
  }
  message "mission-run/${ST_MISSION_RUN}/handoff" {
    status "read"
  }
}
```

Products can match `resource`, `message`, `agent`, `exec`, or `pty` subjects. Each product can require scalar fields.

All products in one block must hold.

A step product is intermediate output for that step. A mission product is a final contract for the complete normal phase.

The worker creates or observes products. st verifies them. The `produces` keyword does not perform the action.

A worker-submitted step stays `verifying` until its products hold. Once the submitting worker's
turn has ended, st sends that worker one message per step attempt. The message names the exact
product subject and fields the step waits for, so a worker that recorded the wrong subject can
correct it.

A mission product can refer to output created during any step. Do not duplicate a step product at mission level unless the same graph subject is intentionally both an intermediate and final contract.

## Gates

A gate decides whether a completed work boundary can pass.

Missions and steps use repeated flat nodes:

```kdl
gate "the artifact exists" { exists "resource/build-artifact" }
gate "the report is green" { field "status" "resource/report" "is" "green" }
```

Sibling gates form an AND relation. There is no `gates` wrapper.

Step gates run after direct declarations, worker report, nested work, used mission, and products hold. Mission gates run after every normal step and all mission products hold.

Each running gate records `gate.requested` and `gate.result`. The result cites operation evidence. A pass releases the boundary. A failure fails the step or mission. A pending graph or human gate keeps the boundary pending.

### Predicate gates

```kdl
gate "subject exists" { exists "resource/result" }
gate "run has no live runtime" { empty "mission-run/${ST_MISSION_RUN}" }
gate "field matches" { field "status" "resource/result" "is" "green" }
gate "prefix matches" { field "revision" "resource/result" "starts-with" "git:" }
gate "text contains value" { has "doc/report@SHA256" "GREEN" }
gate "text omits value" { lacks "message/report" "UNVERIFIED" }
```

`field` uses this argument order: path, full subject, operator, value. Operators are `is`, `starts-with`, and `contains`.

Use `every` to apply one or more field predicates to every item in an observed list:

```kdl
gate "every check passed" {
  every "checks" "resource/github/acme/app/pull/42" {
    field "status" "is" "completed"
    field "conclusion" "is" "success"
  }
}
```

The outer arguments are the list path and full subject. Each nested `field` path is relative to one
list item and uses path, operator, value. All nested fields must match every item. `not-every` has the
same shape and passes when at least one item does not match.

A missing subject, missing path, or non-list value keeps both predicates pending. `every` follows
universal quantification and therefore passes for an empty list; `not-every` remains pending for an
empty list. Use these quantified predicates anywhere deterministic graph predicates are accepted,
including baselines, dependencies, loop exit gates, and ordinary gates.

`has` and `lacks` accept file, document, or message subjects.

A mission gate can also use `deadline "10m"`. A step uses its `timeout` property instead.

### Mechanical gates

```kdl
gate "the tests pass" {
  exec "cargo test --workspace"
  host "local"
  workspace "${ST_WORKSPACE}"
  env { RUST_BACKTRACE "1" }
  time-limit "10m"
}
```

A mechanical gate requires `exec`, `host`, and `workspace`. `env` is optional. `time-limit` defaults to two minutes.

The command runs through the supervised exec runtime. Its result is attempt-bound and durable.

### LLM gates

```kdl
gate "the migration preserves the public contract" type="llm" {
  model "gpt-5.6-sol"
  host "local"
  workspace "${ST_WORKSPACE}"
  tools "shell" "git"
  token-budget 12000
  time-limit "10m"
  prompt "Inspect the diff and evidence. Return PASS or FAIL with a reason."
}
```

An LLM gate requires an explicit model, host, workspace, tool list, positive token budget, time limit, and prompt.

Registered tools are `shell`, `git`, `gh`, and `network`.

The gate fails if its structured usage exceeds the declared token budget.

The gate also fails if it outlasts its time limit. This holds even after the runner has posted a
verdict: a runner that never exits is stopped at the limit.

### Human gates

```kdl
gate "the release is approved" type="human" {
  reviewer "person/alex"
  question "Is the release ready?"
  review "resource/release-candidate"
  review "doc/release/report@SHA256"
}
```

A human gate requires a full `person/...` reviewer. The question and repeated review targets are optional.

st creates one `gate.requested` claim for the exact mission or step revision and attempt. A review decision must match that request.
A newer st build can word the same gate's request differently and ask again for the same attempt.
The reviewer then sees one review: the newest request, which is the one the gate waits on, aged
from the first request, on every node.

`st attention ls --as person/NAME` shows the selected person's pending KDL human gates together
with their other current decisions and faults.

The human view shows the mission, owner step, question, review targets, age, and exact decision commands. `--json` returns the same current review records as structured data.

The list excludes resolved requests, old generations, changed definitions, old attempts, and terminal owners. A result from a different actor does not resolve a request.

### Human attention inbox

`st attention ls --as person/NAME` combines every current item that needs that person. It includes
human gates, launch approvals, revision approvals, unread person messages, and explicit fault
requests. `--json` returns typed records and exact action argument arrays. The person argument is
required and is never inferred from ambient process state.

The formatted view shows each item in oldest-first order. It includes the item kind, age, graph context, targets, and safe commands.

A planning item appears only for the current valid preview. A preview with blockers does not appear. A revision item appears once for each reviewer who has not approved it.

A person message remains until its `message.read` claim. Reading a sent message records delivery first. Archiving is separate and is not required to clear attention.

An explicit fault uses the `attention/ID` subject family. `attention.requested` stores the reviewer, title, reason, severity, and optional targets. `attention.resolved` records a `resolved` or `dismissed` outcome.

Only dedicated attention commands create these fault requests. The runtime does not convert every diagnostic into human work.

## Dependencies

`depends-on` is the only step ordering language.

```kdl
depends-on {
  step "build" completed
  step "test" terminal
  field "status" "resource/change-window" "is" "open"
}
```

A step dependency can require `completed`, `failed`, or `terminal`. `completed` is the default when the state is omitted.

Graph predicate dependencies accept the same deterministic predicates as baselines. They latch after they pass. A later graph change does not move active work backward.

Dependencies inside a nested mission refer to sibling steps in that nested mission.

### Waiting for another run

`after-run` makes a step wait for another mission run:

```kdl
step "wait-for-build" {
  after-run "${input.build_run}"
}
```

The step is agentless and cannot name an agent. It completes when the named run completes. It fails when that run fails or is cancelled. Other steps order after it with `depends-on`. The value is a run ID or `mission-run/` subject, and it can use inputs.

To make one run wait without changing its mission, start it with `after`. See [Starting after another run](kdl-lifecycle.md#starting-after-another-run).

## Runtime sequence

Each run starts with all steps in pending state.

For a normal step, st performs this sequence:

1. Wait for the normal phase.
2. Wait for the parent nested step, when present.
3. Wait for every explicit dependency.
4. Evaluate all step baselines.
5. Resolve the nearest work selector.
6. Verify that at least one eligible agent is present, when the selector names agents.
7. Mark the attempt ready and increment its readiness epoch.
8. Materialize its direct declarations.
9. Wait for declaration convergence.
10. Wait for a worker report when an agent claimed the step.
11. Wait for nested mission steps or an exact used mission.
12. Verify products.
13. Evaluate gates.
14. Mark the step completed or failed.

When a step retry permits another attempt, st increments the attempt, applies backoff, and starts at dependency admission.

Each attempt can submit or fail its work once. A failed gate can return the step for another retry attempt.

`ST_ATTEMPT` contains the current step attempt. The step subject stays stable across all attempts.

A retryable failure does not terminate the mission before the next attempt.

A step has one attempt by default. st does not repeat a failure automatically: a failed attempt is
usually an agent's judgment or a false gate, and a repeated attempt repeats its side effects. Declare
`retry` on a step whose failure is known to be transient.

A person, or an agent with `revise` authority for the mission, can retry one failed step:

```sh
st work retry STEP_RUN --as person/operator --reason "the deploy check host is back"
```

While its run is active, the step starts its next attempt in place. When that step is the only
reason its root run failed, the retry reopens the run in a successor generation of the same
revision. Completed normal work carries forward. The failed step starts its next attempt. Work that
the failure cancelled and every final step start again. The old generation becomes superseded.
The run's state claims record who reopened it and why, and its state dates from the reopening.

A failed run does not reopen when several steps failed, when work was cancelled for a reason other
than the failure, when a mission gate or the mission timeout failed it, when it is a nested or eval
run, or when its mission deadline has passed. Revise the run instead to restart several failed steps.

A finished root run can show another outcome than the one st recorded, for example when its work
shipped after a gate failed. A person, the agent that requested the run, or an agent with `revise`
authority for its mission sets it with a reason:

```sh
st missions outcome RUN completed --reason "the change merged after the gate was fixed" --as person/operator
```

The outcome is `completed`, `failed`, or `cancelled`. The run keeps its steps as they ended. Its
latest state claim records the new outcome, who set it, and why; `st missions show` prints it with
the outcome it replaced, and every client shows it on every node. An active run is cancelled with
`st missions cancel` instead, and nested and eval runs keep their outcome.

The `completion` frontier selects when st checks mission products and gates. st then enters the final phase when one exists.

The run reaches `completed` after successful final work. A final failure makes the run failed.

When `completion` is omitted, all normal steps exhausted selects completion. A mission stays
nonterminal only while work, products, gates, finalization, or a declared completion frontier is
still unresolved.

## Nested missions

A nested `mission` is part of its parent mission revision.

The parent step starts the nested roots after the parent is active. Nested steps inherit the nearest work selector unless a child overrides it.

Nested work remains durable graph state. It is not stored only in harness memory.

## Produced and used missions

A step can publish one complete ready mission as an attempt-bound output.

```kdl
step "compile-mission" {
  assigned-to "agent/planner"
  document "doc/project/mission@SHA256"
  produces-mission "project-work"
}
```

The worker must hold the producing step or one of its nested steps with the same assigned agent.

```sh
st work publish-mission step-run/GENERATION/compile-mission generated.kdl --as agent/planner
```

The published document must contain exactly one ready mission with the declared ID. st publishes the immutable revision and binds it to the producing definition and attempt.

A later step can start that exact output:

```kdl
step "execute-mission" {
  assigned-to "agent/planner"
  depends-on { step "compile-mission" completed }
  uses-mission output-of="compile-mission"
}
```

The output form requires an explicit completed dependency on the producer.

A step can also use an already published exact revision:

```kdl
uses-mission "project-work@REVISION_SHA256"
```

A used mission starts one linked child run. The wrapper completes only after the child completes. A failed or cancelled child fails the wrapper.

## Automatic context

st supplies these exact context names:

| Name | Value |
| --- | --- |
| `ST_MISSION` | Mission ID. |
| `ST_MISSION_REVISION` | Active mission revision hash. |
| `ST_MISSION_RUN` | Stable mission run ID without the `mission-run/` prefix. |
| `ST_RUN_GENERATION` | Current generation ID without the `run-generation/` prefix. |
| `ST_ROOT_MISSION_RUN` | Full root `mission-run/...` subject. |
| `ST_ROOT_MISSION_RUN_ID` | Root mission run ID without the `mission-run/` prefix. |
| `ST_WORKSPACE` | Absolute run workspace. |
| `ST_REQUESTER` | Normalized requester subject. |
| `ST_STEP` | Step path in a step context. |
| `ST_STEP_RUN` | Full step-run subject in a step context. |
| `ST_ATTEMPT` | Current attempt number in a step context. |
| `ST_ASSIGNEE` | Fixed `assigned-to` agent, or an empty value for pools and agentless work. |
| `ST_PARENT_STEP_RUN` | Parent step-run subject, or an empty value. |
| `ST_GATE` | Gate name in a running gate context. |
| `ST3_SUBJECT` | Full subject of the current runtime member. |
| `ST_AGENT` | Full owning agent subject. It is absent for agentless runtimes. |
| `ST3_BIN` | Absolute path to the exact st executable that started the runtime. |
| `ST_LOOP_ROUND` | Current loop round. It is present in loop child missions. |
| `ST_LOOP_FEEDBACK` | Exact prior feedback document, or an empty value. |

The mission and step values are available for `${NAME}` KDL interpolation when the current context defines them. Step members and running gates receive those values as environment variables.

Loop KDL can also use `${loop.round}` and `${loop.feedback}`.

Publication rejects an unknown `${NAME}`. Write `$${NAME}` for the literal text `${NAME}`, for example in a goal that quotes a shell variable. st removes the first `$`.

`ST3_SUBJECT`, `ST_AGENT`, and `ST3_BIN` are runtime-only values because they depend on the materialized member.

For example, use `${ST_MISSION_RUN}` directly. Do not write a manual mapping such as `env { MISSION_RUN "${ST_MISSION_RUN}" }` only to rename the built-in value.

The exact built-in names are reserved in authored `env` maps. st rejects an attempt to replace them. Other names, including other `ST_*` names, remain available to applications.

`${PATH}` is also available for KDL interpolation. Gates and exec steps use the account's
interactive login-shell environment, just like agents. Programs resolve through that captured
PATH, so tools installed by shell startup files do not need absolute executable paths.
Declared environment values override the snapshot; `${PATH}` in an override expands against
the captured shell PATH. The active st executable directory is prepended for launched work.

The daemon captures this environment at startup and refreshes it on use every 60 seconds,
including shell configuration and exported credential changes. `st doctor` reports its PATH
and whether GitHub observers have a token, without displaying credential values. Observers
check `GH_TOKEN`, `GITHUB_TOKEN`, then `gh auth token` on every poll. If none supplies a token,
the observer records an explicit authentication failure and sends no anonymous request.
Run `gh auth login` as the daemon account or export a token in its shell startup files;
the next poll retries authentication.

`st doctor` also has a `build-tools` check for hosts that build and gate this repository. It looks
for `cargo`, `rustc`, `sccache`, `gh`, `git`, and `nix` on that captured PATH, plus `mold` on
Linux, where the repository links with it. Then it builds and links a small crate offline
with the same linker setting, giving the build 10 seconds. The check warns, and does not fail, with
what is missing or the last lines of cargo's error, because a host that does not build can still
run the graph. When cargo or rustc is missing, it does not try the build. Install what is missing,
or export its directory from the daemon account's shell startup files; `st doctor` sees the
change within a minute.

An agent receives its own subject in both `ST3_SUBJECT` and `ST_AGENT`. A nested task receives its task subject in `ST3_SUBJECT` and its parent agent in `ST_AGENT`.

An agentless `exec` or `terminal` receives `ST3_SUBJECT` and no `ST_AGENT`.

A CLI process with `ST_AGENT` acts only as that agent. Work actions, conversation read, archive,
send, and reply, and `claim --actor` refuse a different `agent/...` actor. They still accept a
person, exec, or other non-agent actor that the work names.

An unknown variable or a variable that is not available in the current phase is an error.

## Agent start

A native harness starts with no prompt. A started or restarted seat takes no turn until a person types or a graph message is posted.

A harness block cannot declare `prompt`. Parsing refuses it with `harness-prompt-removed`. Put the instruction in a step goal or send the seat a message.

Each native harness driver installs the skill that `st skill` prints before it starts the harness. The skill describes how to use st: messages, work, and attention requests. It contains no mission goal and sets no rules of conduct.

st no longer writes a `.st3` directory into a native harness workspace. For each one, it removes a `.st3` directory that older releases wrote, unless Git tracks something in it, and removes the `.st3/` line from the Git exclude file once no worktree sharing that file still has a `.st3` directory. A declared `render { git-exclude ".st3/" }` adds nothing.

A declared `render` operation that would change a tracked file fails that member’s render transaction and prevents its runtime from starting. Other members continue rendering and reconciling. Render, start, observation, and stop failures appear as the member’s fault in `st agents show` and `st doctor`; a successful pass clears the fault. Stopped and superseded members bypass rendering.

Repeated `git-exclude` operations build on one another. Seats in worktrees sharing a repository’s exclude file contribute their paths to one combined update. Conflicting ordinary file owners are still rejected, without blocking unrelated members.

## Workspace existence

st requires every member workspace to exist before the member starts.

Use an explicit create property when the mission owns creation of that directory:

```kdl
workspace "${ST_WORKSPACE}/generated" create=#true
```

The default refusal prevents a spelling error from creating an unintended directory.

An agent can declare a Git checkout instead. st then creates the agent's workspace as a worktree of an existing repository before the agent starts:

```kdl
agent "parser" {
  workspace "${ST_WORKSPACE}/parser"
  checkout "${ST_WORKSPACE}/repo" base="origin/main" branch="fan-out/parser" remove-at-run-end=#true
  harness "omp" {}
}
```

- The repository and the workspace must be absolute paths after variable substitution.
- When `base` names a remote branch, such as `origin/main`, st fetches it first. When the fetch fails, st uses the repository's current ref and records a `checkout-fetch-failed` warning.
- A new `branch` starts at `base` without upstream tracking. A branch that already exists is checked out as it is.
- A workspace that already exists is used as it is.
- When the checkout fails, the agent does not start. st records a `workspace-unavailable` diagnostic and retries after 30 seconds.
- With `remove-at-run-end=#true`, st removes the worktree after the agent's run ends and its runtime stops. The branch stays in the repository.
- st keeps a worktree that has uncommitted or untracked changes, and records a `checkout-kept` warning. It also keeps a worktree whose workspace a current member still uses.

[`fan-out.kdl`](../../examples/st3/fan-out.kdl) gives three parallel workers one checkout each.

## Native message delivery and work wake

Maintained harnesses receive graph messages through their native driver boundary. Codex uses typed
app-server turn requests. Claude uses one persistent stream-JSON process and acknowledges the
exact replayed user turn. Pi and OMP acknowledge through their loaded native extensions and
steer a message into a running turn at its next tool boundary. Codex, OpenCode, Pi, and OMP
receive one `<smalltalk-message>` element per message:

```text
<smalltalk-message id="ID" from="SENDER" to="RECIPIENT" subject="TITLE" sha256="BODY-SHA256" graph="message/ID">
bounded one-line body preview
</smalltalk-message>
```

`sha256` is the lowercase hex SHA-256 of the complete message body in the graph, and
`st conversations read message/ID` shows that body. Every attribute value and the preview are
XML-escaped, so sender text cannot close the element or add an attribute. A truncated preview ends
with `…`, and a note after the closing tag says so. Claude receives the plain
`[PING from st3] message/ID from SENDER: TITLE` notice and preview inside its own channel tag. OpenCode
acknowledges the assistant turn whose `parentID` is the exact stable user-message ID. Copying a
message into an inbox or successfully writing transport bytes is not delivery. st advances the
graph only from the durable provider receipt and never injects text or Enter into a terminal
composer.

Terminal control is reserved for diagnosed emergency recovery against an exact current
incarnation. It is not a messaging or work-wake transport.

When an exactly assigned step becomes ready, the reconciler sends a durable work message for the
current harness incarnation. Each agent has one work seat across mission runs: a claimed,
working, or verifying step occupies it. A parent that its agent submitted while one of its own
nested steps is still ready does not occupy the seat; that nested step is woken. Ready steps wait
in the seat queue described below. Only the seat's next work is woken when the seat is free.
Queued steps do not consume wake attempts or arm retry timers while the agent is busy. Delivery is
acknowledged by a new working turn, by a native read or close, by a delivery into a turn that is
still working, or by claiming the step. Pi and OMP steer a wake into the running turn, so a
turn that started before the wake still acknowledges it. An unacknowledged delivery is retried
after 15 seconds, at most three times. Exhaustion writes a `work-wake-exhausted` harness
diagnostic naming the step, incarnation, and attempt count.

## Seat queues

Each agent seat has one ordered queue of the mission runs that have steps assigned to it. A run
joins the end of the queue when it first has a step for that seat, and it leaves when the run is
terminal. A new generation from a revision keeps the run's place.

The seat's next work is the first ready step, in queue order, that is assigned to the seat. A run
whose steps for the seat are waiting on a gate, a dependency, or another seat is passed over, so
it never blocks the runs behind it. It becomes next again as soon as it has a ready step. Inside
one run, `depends-on` and `queue {}` still decide which steps are ready, and ready steps of the
same run keep creation order. `st agents show`, `st agents ls --enrich`, and the reconciler's
work wake all use this one selector.

The queue matters most for a durable top-level seat that serves many runs. A mission-scoped seat
normally serves one run, so its queue has one entry.

```sh
st agents queue agent/example/example/worker
st agents queue move agent/example/example/worker mission-run/release/2026-09-26 --top \
  --reason "the release needs this first" --as person/operator
st agents queue move agent/example/example/worker mission-run/docs/2026-09-26 \
  --after mission-run/release/2026-09-26 --as person/operator
```

`st agents queue AGENT` shows the step the seat holds now, its next work, and then each queued
run in order with its state: `claimed`, `ready`, or `waiting`. `st missions queued AGENT` is the
same show command reached from the `missions` group. A move places one run at the top, at the
bottom, or directly before or after another queued run. It needs explicit person authority, like
other client mutations.

Each move writes one `agent.queue.moved` claim on the agent subject with the run, the placement,
the optional anchor run, the optional reason, the actor, and the graph time. Replicas rebuild the
same order from the same replicated claims: joins apply in graph time, and each move applies after
the joins recorded before it. The queue view lists the recent moves, newest first, so every order
change has an author and a time.

A move changes only which run is next. It never releases, reassigns, or interrupts a step the seat
already holds; the new order applies when the seat is free again.

`st work show STEP` exposes ready age, assignee state, wake attempts, acknowledgement, and failure.
An operator can request another delivery through the same driver path with:

```sh
st work wake STEP --as person/operator --reason "retry native delivery"
```

The assignee may also wake its own step. Another agent needs `queue-authority` for the assignee's
seat. Manual wakes are recorded in the inbox but do not consume the three automatic retry
attempts shown by `st work show`.

Generic terminal programs without a maintained native driver do not have an automatic wake path.

A provider or runtime fault creates a `harness.diagnostic` claim. The roster and mission views show the fault.

The runtime can also send one fault message for a new diagnostic epoch. st does not require a special supervisor, root, or chief-of-staff agent.

## Agent grouping

`under` is repeatable agent metadata.

```kdl
agent "researcher" {
  under "lead" reason="the lead combines the research"
  under "design-group"
  workspace "/work/research"
  command "research"
}
```

A bare target inside a mission uses the same mission run. A full external agent subject stays full.

The relation is visible in `st agents --json`, status, and assigned work. It is suitable for a tree or graph UI.

The relation does not create permission, lifecycle, scheduling, or mandatory reporting behavior.

Missing targets, self-relations, and cycles create warnings during preview. They do not block publication or another agent.

## Host documents

A root host declaration can repeat exact document references:

```kdl
host "local" {
  document "doc/hosts/local@SHA256"
}
```

The host document gives stable host facts to agents on that host. It must not contain current work.

st writes no file for a host document. `st work claim` on that host prints each exact document after the claimed step, under a `HOST  NODE` heading, each under its exact reference.

Publication fails with `missing-document` until the exact document bytes exist in the local store.

A bare document name is invalid in a host declaration. A later version needs a new hash and a new declaration.

## Work selection

A mission or step can declare one agent selector kind. Only a step can declare `agentless`.

```kdl
assigned-to "agent/${ST_MISSION_RUN}/only-worker"
```

`assigned-to` means that only the named agent can claim the work.

```kdl
available-to "agent/${ST_MISSION_RUN}/worker-a"
available-to "agent/${ST_MISSION_RUN}/worker-b"
```

`available-to` creates an explicit pool. The first eligible claim wins one step atomically.

The same agent can claim multiple ready steps. A pool does not impose a one-step limit.

```kdl
agentless
```

`agentless` means that the reconciler performs the work without an agent claim. Subgraphs and gates can complete an agentless step.

A local selector replaces the inherited selector. It does not add to it.

The inheritance order is the step, its mission, its parent step, and its parent mission. The nearest selector wins.

A step without an explicit or inherited selector is agentless.

A duplicate pool member is invalid. Combining selector kinds in one mission or step is invalid.

Publication refuses a selector that names an agent nothing declares; see [References that must resolve](#references-that-must-resolve). When a declared agent later goes away, a step blocks only when none of its eligible agents exist in desired state.

## Work commands

Claimed work uses a renewable claim bound to the agent identity and runtime incarnation.

A nested work action renews active ancestor leases held by the same agent incarnation.

```sh
st work ls --as agent/RUN/node.worker
st work show step-run/GENERATION/step
st work claim step-run/GENERATION/step --as agent/RUN/node.worker
st work progress step-run/GENERATION/step --summary "The tests are running."
st work complete step-run/GENERATION/step --summary "The product is published."
st work fail step-run/GENERATION/step --reason "The compiler rejected the source."
st work release step-run/GENERATION/step --reason "The work needs another owner."
```

The default work list shows ready, active, and blocked work. It summarizes waiting and terminal work.

Add `--all` to show waiting and terminal work. Add `--json` to keep the stable machine view for the selected set.

The default `work show` and `work claim` output gives a human-readable step view. It keeps every actionable subject exact.

A worker completion report is not a correctness result. Products and gates still control final completion.

The native driver renews active claims and transports messages. The reconciler creates one durable Small Talk message for each readiness epoch and runtime incarnation.

A pool message closes when another agent wins the claim. Release or expiry creates a new readiness epoch and a new message.

The work queue is authoritative. A notification only tells an agent that the queue might contain new work.

The reconciler does not send periodic reminders. An undelivered message remains pending. A daemon or harness restart recreates the message only for a new runtime incarnation while the work remains ready.

## Durable agent seats and cancellation

Declare a durable seat directly at the publication root:

```kdl
version 2
agent "example/cos/standing/cos" {
  host "local"
  workspace "/work/cos"
  restart "always"
  fresh-context
  harness "claude" { model "opus" }
}
```

The subject is exactly `agent/example/cos/standing/cos`; placement does not change its identity.
The seat's bare `fresh-context` node starts a new harness session before each step it claims, even when the step has no `fresh-context` node. Omit it when the seat should retain context across ordinary steps.
Typed harnesses always run their real interactive TUI in a PTY. Claude always loads the native st
channel. Use `exec {}` for non-interactive provider commands.

Use `st agents apply FILE --as person/NAME` for authored KDL or `st agents start ...` as a
convenience. `st agents new NAME --host HOST --attach` declares a new seat with the fleet's
harness defaults, waits until its harness is ready, and attaches from any fleet host.
`--print-kdl` prints the exact declaration. `st agents stop SUBJECT` publishes an explicit root
stop.

A mission run does not stop because a controller deletes its runtime. The graph must publish cancellation.

```kdl
version 2
mission-run "RUN_ID" {
  cancellation "request-withdrawn" {
    reason "The request was withdrawn."
  }
}
```

Cancellation revokes active claims and cancels normal work. It then runs the adjacent `finally` graph.

A terminal normal step failure does the same when the mission has an explicit completion rule.

Cancellation also cancels active descendant mission runs. Each descendant uses its own final phase.

The terminal state is `cancelled` after successful final work. A final failure makes the run failed.

After final work, st enters cleanup and stops every runtime owned by the mission run.

The run becomes terminal only after those runtime subjects report a stopped, absent, or exited state.

An exact repeated cancellation is idempotent. The old run and its immutable generations remain readable.

st sends a cancellation message to each active claimant. The message tells the agent to stop that step.

## Continuous missions

A continuous mission stays open after its current steps are exhausted. It does not need a separate mission type.

A recurring schedule creates a durable request for one exact finite mission revision.

```kdl
schedule "cycle" {
  host "local"
  every "6h"
  anchor "2026-01-01T00:00:00Z"
  catch-up "latest"
  work {
    mission "fabric/cycle@REVISION"
    workspace "/work/fabric-cycles"
  }
}
```
For one run per local calendar day, use an IANA timezone instead of a fixed-duration UTC interval:

```kdl
schedule "daily" {
  calendar { at "08:00"; timezone "Europe/Berlin" }
  catch-up "latest"
  work {
    mission "fabric/cycle@REVISION"
    workspace "/work/fabric-cycles"
  }
}
```

For a weekly cycle, use `calendar { at "Mon 09:00"; timezone "Europe/Berlin" }`
inside the same schedule shape. The supported weekday names are `Mon`, `Tue`, `Wed`,
`Thu`, `Fri`, `Sat`, and `Sun`. `calendar` accepts daily `HH:MM` or weekly
`DAY HH:MM` in 24-hour local time plus an IANA timezone. It cannot be combined
with `at`, `every`, or `anchor`. Absolute UTC `at` still fires once; `every` with
a UTC `anchor` still measures fixed elapsed intervals. Daily and weekly calendar
occurrences use the matching local date (`YYYYMMDD`) as their durable key.
A nonexistent wall time in a spring DST gap fires once at the first valid instant after
the gap (02:30 Europe/Berlin on 2027-03-28
fires at 03:00 local). A repeated time in an autumn fold fires once at the earlier instant
(02:30 Europe/Berlin on 2026-10-25 fires at 02:30 CEST, not again at 02:30 CET).

The timezone rules are bundled with the daemon's `chrono-tz` dependency. Upgrading the
daemon with newer timezone data can change the resolved UTC instant of an occurrence not
yet scheduled; once an occurrence is recorded as scheduled, its claimed UTC instant remains
authoritative across restarts and upgrades. The local-date key does not change with the
offset. On first publication, the earliest candidate is today's local date for
daily rules or the next matching weekday for weekly rules; missed occurrences
afterward follow the declared catch-up policy.


The runtime gives each occurrence a deterministic mission run and a unique workspace below the declared root.

The mission steps are normal claimable work. A schedule does not start another occurrence while its prior mission run remains active.

Only the host that requested an occurrence's work starts it. The request can name a mission revision
or owner run that has not reached that host yet. Then it stays pending, and the schedule records a
`reconcile.fault` naming the cause. A request that cannot start for any other reason records
`schedule.work-failed`, and the schedule fires again at its next occurrence.

Each finite cycle can be a nested mission. The parent mission keeps the stable agent and the cycle history.

Use `catch-up "latest"` when a restart must create at most one missed wake occurrence.

With `catch-up "all"`, more missed occurrences than `max-catch-up` hold the schedule rather than
start a burst of work. The schedule records a `reconcile.fault` saying so. Raise `max-catch-up`, or
choose `catch-up "latest"` or `"skip"`, to release it.

Stopping a schedule (`schedule "NAME" { stop }`) cancels the work it requested that has not started.
Each such request records `schedule.work-failed` with the code `schedule-stopped`, so declaring the
schedule again never starts work from before the stop.

The schedule does not assign work. The referenced mission defines its work selectors.

## Mission revisions

Revision authority comes from agent placement in the current generation.

- A direct agent in a step can revise that step subtree.
- A direct agent in a mission can revise the complete mission.
- A direct agent adjacent to missions can revise those missions.

The run requester and any person can propose any revision. A work selector does not grant revision authority.

An agent also needs explicit mission operation authority in its current desired declaration:

```kdl
agent "planner" {
  workspace "${ST_WORKSPACE}"
  harness "codex" {}
  mission-authority {
    publish "project/generated"
    start "fleet/fabric/*"
    revise "fleet/fabric/*"
  }
}
```

Each rule accepts an exact mission ID or a terminal `/*` namespace. The value omits the `mission/` subject prefix.

A mission-scoped agent, such as this planner, receives no mission authority by default. A
top-level seat named `fleet/PROJECT/...` that a person declared holds `publish`, `start`, and
`revise` for `fleet/PROJECT/*` unless its declaration carries `mission-authority`; see
[agent mission authority](kdl-lifecycle.md#agent-mission-authority). `publish`, `start`, and
`revise` are separate permissions.

Mission publication requires a claimed step with the exact `produces-mission` declaration. The agent must use `st work publish-mission`.

Mission start requires `start` authority. Mission revision requires both `revise` authority and existing structural revision authority.

The daemon reads authority from the current desired agent. A candidate mission cannot grant authority to its publisher.

Persons and internal system actions keep their existing authority. System starts from `uses-mission`, schedules, and subscriptions are unchanged.

This check protects a trusted local runtime. Caller identity is not cryptographically authenticated, so `--as` remains a trusted-operator boundary.

st checks the current generation. A candidate cannot add itself as an owner and use that new authority.

`revisions="human-only"` protects a mission or step. The protection is inherited by nested steps.

`revision-reviewer="person/NAME"` selects the human reviewer. The run requester is the reviewer when this property is absent.

All distinct reviewers for the changed paths must approve. Each approval names the exact proposal preview token.

```sh
st work revise MISSION_RUN replacement.kdl \
  --as agent/RUN/worker \
  --reason "The generated source adds one verification step."

st work revision show MISSION_RUN
st work revision approve PROPOSAL PREVIEW_TOKEN --as person/reviewer
st work revision cancel PROPOSAL --as person/reviewer --reason "The request changed."
```

A run can have one pending proposal. A second proposal fails until the first proposal is applied or cancelled.

A failed root run can be revised when its failed steps are the only reason it failed. It has no
active work to drain, so an unreviewed revision cuts over immediately and an approved proposal cuts
over on its final approval. The successor generation reopens the run: compatible completed normal
work carries forward, each unchanged failed step starts its next attempt, and changed, cancelled,
and final work starts again.

### Deferred declarative revision intent

A future KDL operation can propose a produced mission revision against one live mission run.

The operation should compile into the existing revision proposal claims. It must not create a second revision or generation model.

Publishing a mission revision alone must not move an active run. The declaration must identify the target run and exact revision.

A parent mission could target a linked child run. This would let a controller publish the parent mission from a shell heredoc.

The mission could then sequence the proposal and verify the successor generation with normal dependencies and gates.

A human-only approval must remain an external authorized claim. Publishing the controlling KDL must not imply that approval.

This direction is deferred. The first design must define target selection, authority, idempotency, cancellation, and failure behavior.

### Immutable generations

Each accepted revision creates one immutable successor generation. The stable mission-run subject points to the current generation.

The cutover transaction creates generation-specific step-run subjects. It also marks the old generation as superseded.

```sh
st work revision generations MISSION_RUN
st work revision generation RUN_GENERATION
```

st compares normalized step definition hashes. A changed step and every transitive dependent start without prior completion.

Every compatible state carries to the successor. Compatible claimed, working, or blocked work keeps its
worker lease, so the worker continues without claiming again. Compatible verifying work that its
worker has not submitted restarts in ready state.

The old generation remains readable. A late work action against it fails with `stale-run-generation`,
except that the holder of a carried lease reaches the successor step through the old subject.

Mission and step members record their owner run and generation. The reconciler stops members left only in the superseded generation lineage.
It first re-owns each member that the successor still declares, so a revision does not restart a
member it keeps.

Observers, subscriptions, and schedules that the successor no longer declares become stopped
declarations at cutover, as they do at run cleanup. Until then they observe, deliver, and start
nothing. A stopped subscription cancels each request it recorded but never started. Runs it already
started continue.

A compatible member keeps the same run-local subject in the successor. A mission revision cannot move it to another mission run.

A cutover cancels active descendant mission runs that started from predecessor steps. `when-idle` also waits for claimed, working, or verifying descendant work before cutover.

The default `restart-active` cutover creates the successor after approval. Open work messages for the old generation close as superseded.

`revision-cutover="when-idle"` changes the run phase to `revision-draining`. Existing active work can settle, but no new work can claim.

The reconciler creates the successor after the active work reaches a stable state. Cancellation returns the run to its normal phase.

The run keeps its initial revision and root revision. Its current revision is the revision of its current generation.

## Immutable documents

`document` on a step always requires `doc/NAME@SHA256`.

```sh
st documents put request.md --as doc/project/request
st documents get doc/project/request@SHA256 --output request.md
st documents ls doc/project/request
```

Bare document names can appear in an intent before preview. The preview resolves them to the current exact hash. Apply validates the bytes and binds that exact version.

Do not store credentials or raw private measurements in Git or shared st documents.

Store a summary, a redacted sample, or a hash when later work needs durable evidence. Keep raw private data in a restricted external store.

## Review, approve, and start

`st missions publish FILE --as ACTOR` previews and publishes exact authored KDL with authority and
subject-head checks. `st launch preview SESSION` validates a planner candidate, resolves documents,
displays changes, and returns the approval hash.
`st launch approve SESSION HASH --as person/NAME` applies that exact candidate without starting it.
`st launch approve-and-launch` performs the approval and idempotent start as one product workflow.
An authorized agent uses `st work publish-mission`, fenced to its claimed producing step.

### References that must resolve

`missions publish`, `agents apply`, a run revision, and `work publish-mission` refuse, with the code
`unresolved-reference`, a publication that names something st cannot find:

- a pinned mission revision that is not stored on this host, in `uses-mission`, a schedule's `work`,
  or a subscription's `mission` delivery. When the pin is the ID of the claim that published a
  revision, the refusal names the revision to pin instead;
- a mission a subscription delivers to that has no published revision;
- an agent that a work selector, agent grouping, message, or subscription names and nothing declares;
- an observer a subscription names that nothing declares.

The same publication, the graph, or the declarations of the run itself can declare the target. A
reference that contains a value only a run knows, such as a mission input, is not checked.

A publication that declares an agent on this host whose render would fail is refused with
`render-refused`: an operation that would change a tracked file, two operations of one agent that
write different content to one path, or content that disagrees with another agent's render of the
same path.

The preview lists each refusal as a blocker. `st doctor` lists the references already in the graph
that no longer resolve in its `graph-references` check.

`st missions start MISSION --as ACTOR` publishes one mission-run declaration for the current ready revision. Add `--follow` to follow the run until it becomes terminal.

`missions publish` prints each revision it created. Pass that value to `missions start --revision REVISION` to start exactly that revision. A mission published on another host reaches this host by replication. When the mission or the requested revision is not here yet, `start` waits up to 60 seconds with a plain message instead of failing. When a later revision already replaced the requested one, `start` names the replacement and stops. After a run starts, `start` names its revision on standard error and says whether other revisions share the mission name.

`st missions show MISSION_RUN` reads one exact run. `st missions show MISSION` works only when that mission has exactly one nonterminal run.

The default mission view shows the complete run summary and its active graph branch. Add `--follow` to watch an existing run.

Each step shows one line from its worker: the `work complete` summary once submitted, otherwise the latest `work progress` summary. Both come from the current attempt, and `--json` carries them as `completion_summary`, `progress_summary`, and `progress_at_unix_ms`. `st agents show` prints each current step with its latest progress summary and age.

Follow mode redraws one screen on a terminal. It appends each changed snapshot when another program reads the output.

Add `--json` to any mission or work view when a program needs the stable data shape.

The mission shortcut fails when it finds zero or multiple active runs. The error tells the caller to use an exact mission-run subject.

## Planning mode

Planning mode asks one durable Codex harness to author Markdown and KDL for review.

```sh
st launch start --id release-mission request.md \
  --workspace ./project \
  --as person/alex \
  --model gpt-5.6-sol \
  --effort medium

st launch show SESSION
st launch preview SESSION
st launch revise SESSION feedback.md --as person/alex
st launch approve SESSION PREVIEW_TOKEN --as person/alex
st launch cancel SESSION --as person/alex --reason "The request changed."
```

Planning can also prepare a revision for one current mission run:

```sh
st launch start --run MISSION_RUN request.md \
  --workspace ./project \
  --as person/alex

st launch preview SESSION --variant compact
st launch preview SESSION --variant extended
st launch compare SESSION compact extended
st launch propose SESSION extended \
  --as person/alex \
  --reason "The extended variant covers the discovered risk."
```

The planner uses this command:

```sh
st launch submit SESSION --variant compact --markdown MISSION.md --kdl mission.kdl
```

The session stores the request, feedback, Markdown, and KDL as immutable documents. Small Talk carries document references, not mutable file paths.

Each named candidate must contain exactly one ready mission with the requested ID.

A run-targeted session stores the exact source generation and an immutable run context document.

The session can hold multiple draft variants. Proposing one variant rejects a stale source generation.

Preview returns these review values:

- the candidate and mission revisions;
- a static dependency graph;
- the graph subject diff;
- warnings and blockers;
- exact subject tokens;
- one hash over the complete preview.

Revision invalidates the prior preview. Approval requires the current preview token and current subject tokens.

Approval publishes the ready mission and one `planning-session.approved` claim. It does not start a run. Approval and cancellation stop the planner.

Controllers should wait on `planning-session.*` events. They must not spend an agent turn to poll session status.

## Gate results

A running mechanical or LLM gate gets a one-use operation capability. Its internal runner records
the terminal result through `POST /v1/gate-results`; there is no public gate-result CLI escape hatch.
The durable kinds are `gate.requested` and `gate.result`.

## Claims and evidence

Mission execution uses these important claim kinds:

- `mission.published` records an immutable mission revision.
- `planning-session.approved` links an approved launch to Markdown and KDL.
- `mission-run.created` and `mission-run.state` record stable run history.
- `run-generation.created`, `run-generation.superseded`, and `run-generation.state` record revision lineage.
- `revision-proposal.created`, `revision-proposal.approved`, `revision-proposal.cancelled`, and `revision-proposal.applied` record revision review.
- `step-run.carried`, `step-run.state`, and `step-run.retried` record generation-specific step history.
- `mission.produced` binds a generated mission to one producing attempt.
- `gate.requested` and `gate.result` record gate operations and evidence.
- `attention.requested` and `attention.resolved` record explicit human fault attention.

Evidence is a list of claim IDs or immutable graph references that support a result. The evidence does not replace the gate. The gate definition says what must be decided; evidence records why the result is trustworthy.

## Eval contract

Repository integration tests archive explicit fixture directories, post staged documents, apply
version 2 intent, and start the selected eval mission. Eval orchestration is a test boundary, not a
public CLI command.

New top-level eval fixtures belong to the eval run and leave the selected graph during cleanup. An eval can reuse an identical selected declaration. It cannot replace a different selected declaration, such as production host metadata.

The planning-mode eval uses one real Codex planner. A controller waits on the event stream and directly approves the first valid candidate. Mechanical gates prove that the mission was hidden before approval, the preview graph and diff were rendered, the exact hash was approved, one ready mission was published, no run started, immutable documents were linked, the planner stopped, and the workspace did not change.

The launch variant and stale-generation paths are deterministic API tests. They do not spend a model run.

The run-generation revision eval proves an approved cutover, lineage, state carry-over, and automatic generation context. It uses no model run.
