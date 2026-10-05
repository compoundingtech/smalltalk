# Missions in practice

Start with the worker and workspace from [getting started](getting-started.md). A **goal** says what will be true when work is done: “README.md explains the garden and has no broken relative links.” Put procedures in a document the mission names. Keep mission files in the repository so a change to the plan is reviewable too.

## Write, publish, then start

Run these on the machine where `~/st/garden` exists. If you stopped the worker after the first guide, start it again:

```sh
cd ~/st/garden
st agents start garden/worker --as person/ada
mkdir -p docs missions
cat > docs/readme-brief.md <<'EOF'
Prepare README.md for the invented Willow Garden.
Describe the garden in one paragraph and link to garden-note.md.
When checking it, verify every relative link and fix broken ones.
EOF
cat > missions/readme.kdl <<'EOF'
version 2
mission "garden/readme" state="ready" {
  goal "README.md introduces Willow Garden and links to its garden note."
  constraint "Follow docs/readme-brief.md in the workspace."
  step "prepare" timeout="30m" {
    assigned-to "agent/garden/worker"
    goal "README.md contains the introduction and a link to garden-note.md."
  }
  step "approve" {
    agentless
    depends-on { step "prepare" completed }
    gate "Ada approves the README" type="human" {
      reviewer "person/ada"
      question "Is the README ready to use?"
    }
  }
}
EOF
st missions publish missions/readme.kdl --as person/ada
st missions start garden/readme --id garden/readme/one \
  --workspace "$PWD" --as person/ada
st missions show mission-run/garden/readme/one
```

Publication stores a definition; starting creates a run. Wait until `prepare` completes. The human gate keeps this run open while Ada reads the result. The worker is free to serve other runs while the gate waits.

One seat holds one claim at a time. Give independent parallel work separate seats with separate workspaces; declaring two root steps for one seat does not make it work on both at once.

## Make the mission independent of its author

Write every rule and decision you already know into the constraints or the documents the mission names. Its agents should be able to proceed without asking you to repeat them: a mission that depends on its author being present has a point of failure.

Declare order with `depends-on`, `--after`, gates, and subscriptions. Record progress and completion in the graph instead of asking agents to report back by message. Keep agent-to-agent messages for real blockers. When the final result needs a person's decision, finish the independent work and use `st work ask` to present that concrete result for review.

## Add a step to the running mission

Write the **complete replacement mission**. The revision file starts with `version 2` and contains only this mission block, without top-level seats or a `mission-run` declaration:

```sh
cat > missions/readme-v2.kdl <<'EOF'
version 2
mission "garden/readme" state="ready" {
  goal "README.md introduces Willow Garden and links to its garden note."
  constraint "Follow docs/readme-brief.md in the workspace."
  step "prepare" timeout="30m" {
    assigned-to "agent/garden/worker"
    goal "README.md contains the introduction and a link to garden-note.md."
  }
  step "check-links" timeout="20m" {
    assigned-to "agent/garden/worker"
    depends-on { step "prepare" completed }
    goal "Every relative link in README.md resolves to an existing file."
  }
  step "approve" {
    agentless
    depends-on { step "check-links" completed }
    gate "Ada approves the README" type="human" {
      reviewer "person/ada"
      question "Is the README ready to use?"
    }
  }
}
EOF
st work revise mission-run/garden/readme/one missions/readme-v2.kdl \
  --as person/ada --reason 'Check the links before review.'
st work revision generations mission-run/garden/readme/one
st missions show mission-run/garden/readme/one
```

The run keeps its ID and gains a new immutable generation. Compatible completed work carries forward; the added step becomes work. Active claims are released and reissued at cutover, and the seat picks up its step again. Changed work and its dependents may need to run again. Publishing a new definition alone does **not** change an existing run. Protected revisions can instead wait for approval or an idle boundary; see [running mission revisions](../examples/st3/CHANGE-A-RUNNING-MISSION.md).

## Queue the next run

While README review is still waiting, prepare work that should happen after it:

```sh
cat > missions/summary.kdl <<'EOF'
version 2
mission "garden/summary" state="ready" {
  goal "summary.md describes the approved garden README in two sentences."
  step "summarize" timeout="20m" {
    assigned-to "agent/garden/worker"
    goal "summary.md contains a two-sentence summary of README.md."
  }
}
EOF
st missions publish missions/summary.kdl --as person/ada
st missions start garden/summary --id garden/summary/one \
  --after mission-run/garden/readme/one --workspace "$PWD" --as person/ada
st agents queue agent/garden/worker
```

| Need | Use |
| --- | --- |
| One step must follow another in the same run | `depends-on`; source order alone gives no execution order. `queue {}` is shorthand for a sequence of steps. |
| A whole run must wait for another run | `--after RUN`; failure or cancellation of the prerequisite also fails the waiting run. |
| Several independent runs share one durable seat | Its standing seat queue. Runs enter in start order; a blocked run is passed over until it has ready work. |
| New external events should create work | An observer and subscription, such as [repository intake](github-integration.md). The subscription starts finite runs, and the seat queue serves them. |

Inspect a seat queue before moving work. A move changes what comes next and does not interrupt its held claim:

```sh
st agents queue move agent/garden/worker mission-run/garden/summary/one \
  --top --reason 'Put the summary first when its prerequisite completes.' --as person/ada
```

Moving a run cannot satisfy its dependencies. Keep standing availability in a **seat**, not an empty mission: an empty mission finishes immediately. An intake mission that owns subscriptions needs a retirement gate to keep those subscriptions alive. See [seat queues](st3/seat-queue.md) and [subscriptions](st3/resource-subscriptions.md).

## Approve a planned human gate

When `check-links` finishes, the approval appears in Ada's Home and attention inbox. Inspect it and paste the exact `step-run/.../approve` ID when prompted:

```sh
st attention ls --as person/ada
printf 'Step-run ID of the garden README approval: '
read -r approval_step
st attention approve "$approval_step" --as person/ada --reason 'The README and its links are ready.'
st missions show mission-run/garden/readme/one
st missions show mission-run/garden/summary/one
```

That completes the README run and releases the summary run. Rejecting a gate fails it; use a [feedback gate](../examples/st3/human-feedback.kdl) when you want a request for changes to return work to its worker instead.

## Ask for a decision discovered during work

A planned gate belongs in the mission. A choice discovered by the agent belongs in `st work ask`: it creates person work with a durable answer and resumes the asking step after the answer arrives. For example, an agent that cannot infer a file format can send this structured request from its own shell:

```sh
cat > format-choice.json <<'EOF'
{
  "version": 1,
  "type": "choice",
  "question": "Which format should the garden catalog use?",
  "why_person": "The brief does not choose a format; this is Ada's product preference.",
  "subjects": [{"kind": "document", "label": "Garden brief", "ref": "docs/readme-brief.md"}],
  "answers": [
    {"id": "markdown", "label": "Markdown", "consequence": "Create catalog.md."},
    {"id": "json", "label": "JSON", "consequence": "Create catalog.json."}
  ]
}
EOF
printf 'Your currently claimed step-run ID (from st work ls): '
read -r asking_step
asking_incarnation=$(st work show "$asking_step" --json | \
  python3 -c 'import json,sys; print(json.load(sys.stdin)["value"]["claim_incarnation"])')
st work ask --for person/ada --title 'Choose the catalog format' \
  --step "$asking_step" --incarnation "$asking_incarnation" --request format-choice.json \
  --idempotency-key garden-catalog-format --as "$ST_AGENT"
```

Use a stable idempotency key for one question. On returning to work, the agent reads `person_answers` with `st work show "$asking_step" --json` and acts on the answer ID. For a seat with no claimed work, use `--new-run NAME` instead of `--step`. [Talking to agents](talking-to-agents.md) shows decision, choice, and feedback requests and how a person answers.

The explicit incarnation works around [a v0.3.4 Claude shell bug](https://github.com/compoundingtech/smalltalk/issues/1236). Read it from your **own current claim**, after claiming the step; never reuse an incarnation from before a restart or revision.

## Traps and fixes

- `--id` is the **whole run path**: `garden/readme/one` creates `mission-run/garden/readme/one`; `one` creates `mission-run/one`.
- Start a run on the machine where its workspace exists. Replication shares the plan, not the checkout.
- Write a duration as `"90m"`, not `"1h30m"`.
- `${...}` inside KDL text is an st variable. Use it only for supported variables; a quoted shell heredoc preserves it for st to expand. Avoid that spelling for unrelated placeholders.
- Each `constraint` holds at most 1,000 bytes. Keep it short and point to the instructions document.
- A cleanup step needs enough timeout to stop its seat. Use minutes, not a few seconds, and inspect the stop result before considering cleanup complete.

See [KDL lifecycle](st3/kdl-lifecycle.md) and [worked examples](../examples/st3/README.md) for larger plans.

## Inspect work from the CLI

```sh
st now                  # what needs you right now
st agents ls            # seats and other running agents
st missions ls          # missions with current runs
st work ls              # steps that are ready or in progress
st attention ls         # decisions and requests waiting for you
st conversations ls person/ada
```

`st attention approve ID --as person/ada` answers a gate waiting for you by the ID
`st attention ls` prints; `reject` and `request-changes` also take `--reason TEXT`. A gate
that no longer waits says why: who answered it, or what changed since it asked.

`st --help` and `st help` open with the main uses, then group commands for everyday use, agent
seats, and running a machine or fleet. `st help --all` also lists plumbing commands.
Use `st help agents new` to open a command's full help.

Lists show current state. Add `--all` for history. Every command has `--help`, and the global
`--json` flag prints the stable client format that the apps read. Run `stui` for the same views
in a terminal app.

For spontaneous work, `st work start "Inspect the fixture" --as agent/example/worker` opens a
one-step run and prints how to claim it. Use `work progress --summary` for checkpoints and
`work complete --summary --evidence` to close it. `work handoff STEP --to agent/example/reviewer
--note TEXT --as agent/example/worker` releases the sender's claim and delivers a note to the
recipient. The recipient reads it, uses `work acknowledge STEP --message MESSAGE --as RECIPIENT`,
then claims it. A person recipient sees the step on their home and closes with `work done`.
`work show STEP` includes the note and acknowledgment. Start and handoff accept
`--idempotency-key KEY` for retries after a timeout.
