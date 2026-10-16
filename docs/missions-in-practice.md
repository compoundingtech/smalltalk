# Missions in practice

Finish [getting started](getting-started.md) before declaring a worker. A **goal** says what should be true when the work is done: “README.md explains the garden and has no broken relative links.” Put instructions in a document the mission names. Keep mission files in the repository so you can review changes to the plan.

## Declare a worker seat

These examples use the invented person `ada` and a Claude seat named `garden/worker`. Use your configured person name. Install Claude Code and finish its login before applying the declaration.

```sh
mkdir -p ~/st/garden && cd ~/st/garden
cat > worker.kdl <<EOF
version 2
agent "garden/worker" {
  workspace "$PWD"
  restart "always"
  harness "claude" {
    model "sonnet"
  }
}
EOF
st apply worker.kdl --as person/ada
st terminals attach agent/garden/worker   # finish first-run prompts, then Ctrl+\\ to detach
```

The seat starts idle and waits for a message or ready mission step. If you have already declared it and stopped it, run `st agents start garden/worker --as person/ada`.

## Write, publish, then start

Run these in the worker workspace on the machine where you declared it:

```sh
cd ~/st/garden
mkdir -p docs missions
printf 'Willow Garden grows beans.\n' > garden-note.md
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
st apply missions/readme.kdl --as person/ada
st missions start garden/readme --id garden/readme/one \
  --workspace "$PWD" --as person/ada
st missions show mission-run/garden/readme/one
```

Publishing saves the mission; starting it begins a run. Wait until `prepare` finishes. The approval gate keeps the run open while Ada reads the result. The worker can work on other runs while it waits.

A seat can claim only one step at a time. To run separate tasks at once, give each its own seat and workspace. Two steps with no dependencies still run one at a time if they share a seat.

## Let the mission run while you are away

Write every rule and decision you already know into the constraints or the documents the mission names. Agents should not need you to repeat them. If a mission needs you there to answer questions, it stalls when you're away.

Set the order with `depends-on`, `--after`, gates, and subscriptions. Record progress and finished work in the graph, where other agents can read it. Agents should send each other messages when something stops them from continuing. If the result needs a person's decision, finish the work you can do first. Then use `st work ask` to show that result for review.

## Add a step to the running mission

Write the **complete replacement mission**, including the steps you are keeping. Start the file with `version 2`. Include only this mission block, with no top-level seats or `mission-run` declaration:

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

The run keeps its ID and gets a new saved version, called a generation. Each generation stays as written. Finished work carries over if it still fits the new plan. The new step joins the work to do. When the plan changes, active claims are released and issued again, and the seat picks up its step again. Changed steps and steps that depend on them may need to run again. Publishing a new mission alone does **not** change a run already in progress. A protected change may wait for approval or until the current steps finish. See [running mission revisions](../examples/st3/CHANGE-A-RUNNING-MISSION.md).

## Queue the next run

While the README waits for review, prepare the work that should follow it:

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
st apply missions/summary.kdl --as person/ada
st missions start garden/summary --id garden/summary/one \
  --after mission-run/garden/readme/one --workspace "$PWD" --as person/ada
st agents queue agent/garden/worker
```

| Need | Use |
| --- | --- |
| One step must follow another in the same run | `depends-on`; the order of steps in the file does not set the order they run. `queue {}` is a shorter way to write a series of steps. |
| A whole run must wait for another run | `--after RUN`; if the earlier run fails or is cancelled, the waiting run also fails. |
| Someone must hear that a run failed, was cancelled or stalled | `report-to="agent/NAME"` on the mission, or `--report-to AGENT` when starting, or `st missions report-to RUN --agent AGENT --as person/NAME` on a run already going. The agent gets one message for each event. See [run reports](st3/mission-graph-runtime.md#run-reports). |
| Several independent runs share one durable seat | Its seat queue. Runs enter in the order they start. The queue skips a blocked run until it has work ready. |
| New external events should create work | An observer watches for events. A subscription starts a separate run for each matching event. Each run finishes, and the seat queue handles them. See [repository intake](github-integration.md). |

Check the seat queue before moving work. A move changes what comes next. It does not interrupt the step the seat has already claimed:

```sh
st agents queue move agent/garden/worker mission-run/garden/summary/one \
  --top --reason 'Put the summary first when its prerequisite completes.' --as person/ada
```

Moving a run does not finish the work it must wait for. Use a **seat** to keep an agent available. An empty mission finishes immediately. A mission that takes in events through subscriptions needs a retirement gate: this keeps it open until you choose to retire it, so its subscriptions keep working. See [seat queues](st3/seat-queue.md) and [subscriptions](st3/resource-subscriptions.md).

## Approve a planned human gate

When `check-links` finishes, Now shows Ada the approval as an alert. Read it and paste the exact `step-run/.../approve` ID when asked:

```sh
st alerts ls --as person/ada
printf 'Step-run ID of the garden README approval: '
read -r approval_step
st alerts approve "$approval_step" --as person/ada --reason 'The README and its links are ready.'
st missions show mission-run/garden/readme/one
st missions show mission-run/garden/summary/one
```

Approval finishes the README run and lets the summary run start. Rejecting a gate fails it. Use a [feedback gate](../examples/st3/human-feedback.kdl) if a request for changes should send the work back to its worker.

## Ask for a decision that comes up during work

Put decisions you can plan for in the mission as gates. If an agent finds a choice it needs you to make, use `st work ask`. This creates a task for a person and saves their answer. The asking step resumes when the answer arrives. For example, if the brief does not say which file format to use, the agent can send this request from its own shell:

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
st work ask --for person/ada --title 'Choose the catalog format' \
  --step "$asking_step" --request format-choice.json \
  --idempotency-key garden-catalog-format --as "$ST_AGENT"
```

Use the same idempotency key when retrying the same question, so it is only asked once. When the agent returns to work, it reads `person_answers` with `st work show "$asking_step" --json` and uses the answer ID to choose what to do. If the seat has no claimed work, use `--new-run NAME` instead of `--step`. [Talking to agents](talking-to-agents.md) shows decision, choice, and feedback requests and how a person answers.

`--incarnation` defaults to the seat's `ST3_INCARNATION`. A restart or a plan change gives the claim a new incarnation, so claim again before asking.

## Traps and fixes

- `--id` is the **whole run path**: `garden/readme/one` creates `mission-run/garden/readme/one`; `one` creates `mission-run/one`.
- Start a run on the machine where its workspace exists. The fleet copies the plan between machines. It does not copy the checkout.
- Write a duration as `"90m"`, not `"1h30m"`.
- `${...}` inside KDL text is an st variable. Use it only for variables st supports. Quoting the heredoc marker keeps the shell from expanding it, so st can read it later. Use another spelling for other placeholders.
- Each `constraint` holds at most 1,000 bytes. Keep it short and link to the document with the instructions.
- Give a cleanup step enough time to stop its seat. Allow minutes rather than a few seconds. Check that the seat stopped before marking cleanup done.

See [KDL lifecycle](st3/kdl-lifecycle.md) and [worked examples](../examples/st3/README.md) for larger plans.

## Inspect work from the CLI

```sh
st now                  # your alerts right now
st agents ls            # seats and other running agents
st missions ls          # missions with current runs
st work ls              # steps that are ready or in progress
st alerts ls            # decisions and requests waiting for you
st conversations ls person/ada
```

`st alerts approve ID --as person/ada` answers a gate using the ID from
`st alerts ls`. `reject` and `request-changes` also take `--reason TEXT`.
If the gate is no longer waiting, it says who answered it or what changed since it asked.

After changing a person identity, repair its current inbox once. Upgrade every fleet member
to a build that understands person repair first, and send the repair through one member.
OLD and NEW must be distinct complete `person/NAME` subjects. NEW must already be known
from stored identity or routing evidence; naming it in this request does not create it.
Only OLD or NEW may act: agents and unrelated people are refused.

```sh
st persons rename person/ada person/robin --as person/robin \
  --idempotency-key ada-person-repair
```

This atomically reassigns eligible open person steps and pending human reviews on that
member without changing their episodes, original requests, saved specifications, run
requesters, or historical actors. It creates no alias and does not rewrite published
missions. Work created later still follows its declarations.

Use `--json` to keep the complete report:

| Report field | Meaning and next action |
| --- | --- |
| `reassigned_steps`, `reassigned_reviews` | Current steps and review owners moved to NEW. Read NEW's inbox to continue them. |
| `unchanged_attention_requests`, `unchanged_fault_cards` | Current attention requests and fault cards still routed to OLD; these were not moved. Inspect each ID and resolve it through its normal workflow. |
| `unchanged_revision_approvals`, `unchanged_planning_launch_approvals` | Revision-proposal and planning-launch approvals still routed to OLD; these were not moved either. Inspect and handle the existing approval rather than assuming it reached NEW. |
| `live_run_references` | Live run IDs mapped to JSON-pointer paths that still name OLD in their routing or retained specifications. Runtime step assignees use `/step_runs/<escaped-step-subject>/assignee`; saved-spec routes and `/requester` are listed separately. Inspect the run and use an authorized run revision where appropriate; republishing alone does not change a live run. |
| `published_mission_references` | Published mission IDs mapped to JSON-pointer paths still naming OLD, including step assignees and reviewer routes. Update the source declarations and republish before starting future work. |
| `run_requesters`, `missions_to_republish` | Summary IDs for unchanged live-run requesters and published missions with OLD references. Requesters remain historical attribution, not renamed actors. |
| `namespace_notice` | Client-derived custom/person namespaces, including private glasses and terminals, are not automatically moved. No namespace migration or alias is created. |

The report is a snapshot, not a promise that OLD will receive no future work. Review every
unchanged category and routing path. Saved specifications and historical attribution remain
unchanged even when current runtime routing has moved.

After a timeout, retry the **same key and identical request**, including the actor, through
the same upgraded member. The winning durable receipt returns its original report, including
after reopen or replication; it does not sweep work created since that receipt. Reusing a key
for another request returns `idempotency-conflict`. A completed repair fences OLD against a
different destination with `person-rename-conflict` before any routing writes, so competing
requests on that member cannot split the repair.

Independent disconnected members can each initially accept a different destination.
After their receipts replicate, the earliest completed receipt in canonical claim order
(including envelope ordering) selects one destination consistently for both work and review
routing. Losing routes are removed by rebuilding the affected run projections; retrying a
losing receipt's exact request returns `person-rename-conflict` instead of its old report.
The winner's exact retry remains write-free. This is eventual convergence, not global
admission exclusion before replication. Use one member for repair rather than deliberately
issuing competing requests.

Do not try to undo a completed repair by swapping an older binary back in or sending OLD to
another destination. Keep the receipt and claim history, finish updating declarations and
handling the reported unchanged items, and roll forward on compatible builds. Restoring an
older backup loses subsequent claims and requires coordinated fleet recovery, not just a
local binary rollback.

`st --help` and `st help` show the main uses first. They then group commands for everyday use,
agent seats, and running a machine or fleet. `st help --all` also lists commands used internally.
Use `st help agents new` to open a command's full help.

Lists show what is happening now. Add `--all` to see history. Every command has `--help`.
The global `--json` flag prints data in the stable format the apps use. Run `st` to see
the same information in a terminal app.

For work without a mission file, `st work start "Inspect the fixture" --as agent/example/worker` opens a
one-step run and prints how to claim it. Use `work progress --summary` to save progress and
`work complete --summary --evidence` to close it. `work handoff STEP --to agent/example/reviewer
--note TEXT --as agent/example/worker` releases the sender's claim and sends the next worker
a note. That worker reads it, uses `work acknowledge STEP --message MESSAGE --as RECIPIENT`,
then claims the step. If the next worker is a person, the step appears on their home, and they
finish it with `work done`. `work show STEP` includes the note and confirms it was read.
Start and handoff accept `--idempotency-key KEY` so you can retry after a timeout.
