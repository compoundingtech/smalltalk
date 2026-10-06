# Start a new project from nothing

This walkthrough uses two complete files. A seat file declares a durable project worker, and a
finite work mission assigns work to that worker. All names and data are invented. Use a disposable
ST installation with a working login for one harness, from the repository root.

## Failure first: a mission is not a durable agent

The tempting shape for a long-lived worker is a mission that declares an agent and has no steps.
That was the old standing-mission pattern, and it no longer keeps anything alive. Missions are
finite. A mission with no steps has exhausted its work as soon as it starts, so the run completes
and stops the agent it owns. A revision or a second publish of that definition ends the same way.

The second tempting mistake is to publish and start work before any worker exists. Publication
succeeds with a warning, and the step waits with no eligible agent. The warning names the loop's
generated round mission, which holds the step:

```text
mission `mission/__st3/example/garden-work/loop/prepare-note/round` references missing eligible agent `agent/example/worker`
```

A durable agent is a top-level seat, applied with `st apply`. It has no mission owner, so
no mission's end can stop it. Work reaches it as mission steps assigned to its exact subject.

## Supported way: apply a seat, then run work

Create an empty workspace and inspect both files before changing graph state. Pick the seat file
for the harness you have logged in; every file in [`seats/`](seats/) declares the same
`agent/example/worker` seat. Replace its `workspace` with an existing directory first.

```sh
walkthrough_workspace="$(mktemp -d)"
sed -n '1,80p' examples/st3/seats/omp.kdl
sed -n '1,80p' examples/st3/walkthrough-work.kdl
```

For a longer mechanical gate, put its shell in a file and check that file with `/bin/bash -n`
before publication. This example's gate is a single command; its equivalent syntax-only check is:

```sh
/bin/bash -n -c '/usr/bin/test -s garden-note.md # round 1'
```

Then run these four ST commands in order.

1. Apply the seat:

   ```sh
   st apply examples/st3/seats/omp.kdl --as person/operator
   ```

   **This is the step that creates the agent.** Verify it:

   ```sh
   st agents show agent/example/worker
   ```

2. Publish the finite work definition:

   ```sh
   st apply examples/st3/walkthrough-work.kdl --as person/operator
   ```

   Publication stores an immutable ready definition; it does not start a run. The missing-agent
   warning above does not appear now that the seat exists. `st missions ls --all` lists
   `mission/example/garden-work` with zero runs.

3. Start the work and follow it:

   ```sh
   st missions start example/garden-work --id example/garden-work/first-change \
     --workspace "$walkthrough_workspace" --as person/operator --follow
   ```

   The run joins the seat's queue, and st wakes the seat for `write-note`. `--follow` returns when
   the run is terminal.

4. Inspect the result:

   ```sh
   st missions show mission-run/example/garden-work/first-change
   ```

`apply FILE --as ACTOR` previews and publishes exact authored KDL. A person actor is an
explicit trusted local operator. An agent actor is checked against the agent's already-current
`mission-authority`; authority written into the candidate being published cannot grant itself.

## Why the work mission has this shape

The `loop` owns the overall 30-minute budget. Each `round` creates fresh `write-note` work. `until`
checks the result after a round, `max-rounds` prevents an endless retry, and `on-exhausted` fails
with explicit human attention after the third miss. `${loop.feedback}` tells the next round which
until gate failed. Put corrective work inside `round`; put work that should happen once after a
successful loop after the loop and depend on the loop step.

A gate result is cached by the gate's definition, and an `until` gate belongs to the loop rather
than to one round. The gate command therefore ends with the shell comment `# round ${loop.round}`.
st substitutes the round number, so each round runs the check again. Without it, the first
round's failure would be reused for every later round.

Mechanical gates use the captured interactive login-shell environment. Tools resolve through
that PATH; the example's `/usr/bin/test` also works as an explicit path. `st doctor` shows the
daemon PATH. Keep the owning step or loop timeout longer than the gate's
`time-limit`; here 30 minutes exceeds one minute. Syntax-check nontrivial shell with
`/bin/bash -n` before publishing it.

This gate reads a file in a disposable workspace that is not a repository. In a real project, have
the worker commit and push, and gate on the pushed tree instead of the worker's files;
[`WRITE-A-GATE-THAT-WORKS.md`](WRITE-A-GATE-THAT-WORKS.md) shows how.

The run's outcome is graph state. `missions show` gives its status, each step's completion, and
the gate results. A failed loop also puts an attention item in the operator's inbox, so a failure
does not depend on anyone reading a message.

After the run, `garden-note.md` is in the disposable workspace and the seat remains running for
the next mission. Stop it with `st agents stop agent/example/worker --as person/operator` when you
are done. Remove the workspace; immutable definitions and the completed run remain in graph
history by design.
