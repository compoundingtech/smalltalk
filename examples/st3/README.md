# st3 examples

Each KDL file here is a complete, runnable example. Its header comment says what it shows and the
commands that run it. Every file passes the normative st3 parser in the test suite:

```sh
cargo test -p st3 --test examples
```

All names, repositories, paths, and people are invented. Replace them before you use an example.

## How the pieces fit

- **A durable agent is a seat.** Declare it as a top-level `agent` in its own file and apply it with
  `st3 agents apply FILE --as person/NAME`. A seat has no mission owner, so no mission's end stops
  it. Stop it with `st3 agents stop`. Do not model a durable agent as a mission with no steps: that
  run completes at once and stops the agent it owns.
- **Work reaches a seat as mission steps.** A step `assigned-to` the seat's exact subject waits in
  the seat's queue. The seat holds one step at a time and takes the next ready step in queue order.
  Messages are for conversation, not for handing out work.
- **Agents react to graph events.** Steps, `depends-on`, gates, observers, subscriptions, and
  schedules move work forward. Nobody polls in a turn.
- **Results live in the graph.** Complete the step with the result, or store a longer result with
  `st3 documents put` and cite the reference. A report message is invisible to the graph.
- **Missions are finite.** Publishing a mission stores an immutable definition; `st3 missions start`
  starts a run. A mission that owns an observer, subscription, or schedule needs a step that keeps
  its run open, such as the `retire` gate in [`github-intake.kdl`](github-intake.kdl).

Publish exact hand-authored missions with `st3 missions publish FILE --as ACTOR`. For
conversational planning, use `st3 launch start`, review the candidate with `st3 launch preview`,
and approve it with `st3 launch approve-and-launch`. An authorized agent uses
`st3 work publish-mission` while it owns the declared producing step.

## Find an example by task

### Run a durable agent

| Task | Example |
| --- | --- |
| Run an omp seat | [`seats/omp.kdl`](seats/omp.kdl) |
| Run a Claude Code seat | [`seats/claude.kdl`](seats/claude.kdl) |
| Run a Codex seat | [`seats/codex.kdl`](seats/codex.kdl) |
| Run an OpenCode seat | [`seats/opencode.kdl`](seats/opencode.kdl) |
| Run a pi seat | [`seats/pi.kdl`](seats/pi.kdl) |
| Start from nothing: a seat, then finite work | [`WALKTHROUGH.md`](WALKTHROUGH.md), [`walkthrough-work.kdl`](walkthrough-work.kdl) |

Every harness file declares the same `agent/example/worker` seat, and every mission example
assigns work to it, so the missions run with whichever harness you apply. The planner, reviewer,
and chief seat files declare other seats, named for their roles. omp seats run best on
`openai-codex/gpt-6-astra`; [Running st3 with omp](../../docs/st3/omp.md) explains why and lists
the setup.

### Give a seat work and order it

| Task | Example |
| --- | --- |
| Send one mission's steps to several seats, and share a seat between missions | [`many-to-many.kdl`](many-to-many.kdl), [`seats/planner.kdl`](seats/planner.kdl), [`seats/reviewer.kdl`](seats/reviewer.kdl) |
| Serve several missions from one seat, and reorder its queue | [`seat-queue.kdl`](seat-queue.kdl) |
| Let an agent reorder another seat's queue (`queue-authority`) | [`seats/chief.kdl`](seats/chief.kdl) |
| Run steps of one mission in a fixed order | [`queued-work.kdl`](queued-work.kdl) |
| Run nested jobs one after another | [`queued-nested-work.kdl`](queued-nested-work.kdl) |
| Delegate a step to an inline child mission with its own agent | [`nested-mission.kdl`](nested-mission.kdl) |
| Fan work out to parallel mission-scoped seats, each in a worktree that st3 creates and removes (`checkout`) | [`fan-out.kdl`](fan-out.kdl) |
| Review independently and keep remediation reachable | [`review-remediation.kdl`](review-remediation.kdl) |

### Start missions from events and time

| Task | Example |
| --- | --- |
| Start a mission for each new GitHub pull request or issue | [`github-intake.kdl`](github-intake.kdl), [`github-intake-work.kdl`](github-intake-work.kdl) |
| Start a bounded cycle on a schedule | [`recurring-stewardship.kdl`](recurring-stewardship.kdl), [`recurring-stewardship-cycle.kdl`](recurring-stewardship-cycle.kdl) |
| Gate work on an observed local file | [`resource-observation.kdl`](resource-observation.kdl) |

### Wait for something

| Task | Example |
| --- | --- |
| Wait for a pull request's GitHub checks to pass | [`wait-for-green-checks.kdl`](wait-for-green-checks.kdl) |
| Repeat a fix until the pushed branch passes its tests | [`loop-until-green.kdl`](loop-until-green.kdl), [`test-pushed-branch.sh`](test-pushed-branch.sh) |
| Wait until a time | [`wait-until-time.kdl`](wait-until-time.kdl) |
| Wait for a person's review | [`human-review.kdl`](human-review.kdl) |
| Wait on an agent: its work, its runtime, or a run's agents exiting | [`wait-for-agent.kdl`](wait-for-agent.kdl) |
| Write a mechanical gate that works | [`WRITE-A-GATE-THAT-WORKS.md`](WRITE-A-GATE-THAT-WORKS.md), [`gate-recovery.kdl`](gate-recovery.kdl), [`verify-catalog-index.sh`](verify-catalog-index.sh) |

### Change or recover a run

| Task | Example |
| --- | --- |
| Revise a running mission | [`CHANGE-A-RUNNING-MISSION.md`](CHANGE-A-RUNNING-MISSION.md), [`mission-revision.kdl`](mission-revision.kdl), [`mission-revision-v2.kdl`](mission-revision-v2.kdl) |
| Recover a cancelled run whose final work cannot start | [`RECOVER-A-STUCK-RUN.md`](RECOVER-A-STUCK-RUN.md) |

### Talk to people and agents

| Task | Example |
| --- | --- |
| Ask a person for a decision and keep working | [`ASK-A-PERSON-FOR-SOMETHING.md`](ASK-A-PERSON-FOR-SOMETHING.md) |
| Send a message with a subject, a thread, and a large body | [`SEND-A-MESSAGE-PROPERLY.md`](SEND-A-MESSAGE-PROPERLY.md) |
| Find out what a quiet seat is doing | [`FIND-OUT-WHAT-IS-HAPPENING.md`](FIND-OUT-WHAT-IS-HAPPENING.md) |

## Gates that work

Gates run through `sh -c` with a minimal environment and no login shell:

- Use absolute binary paths, such as `/usr/bin/git`.
- `${NAME}` is an st3 variable; write shell variables as plain `$NAME`.
- A gate on files checks the committed, pushed tree, not an agent's working tree.
- A gate result is cached by its definition. A loop's `until` gate puts `${loop.round}` in its
  command, and a step retried until a time puts `${ST_ATTEMPT}` in its gate.
- A graph predicate that is false stays pending; `every` passes on an empty list.

[`WRITE-A-GATE-THAT-WORKS.md`](WRITE-A-GATE-THAT-WORKS.md) explains each rule.

## Failure-first guides

The Markdown guides show the failure before the supported way out. The happy path is usually
discoverable from command help; the expensive mistakes happen after a command surprises someone. A
recurring bad pattern is to hit that wall, assume a capability disappeared, and skip the help for
the exact subcommand. Keep this failure-first shape when editing them; turning them into
field-by-field reference pages would erase their purpose.

Mission goals and constraints describe the work. The generated `.st3/boot.md` describes how every
agent uses st3. Do not copy universal boot instructions into a harness prompt or a mission
constraint.

Keep durable seat and mission KDL in a Git repository, even when a planner authored it.
