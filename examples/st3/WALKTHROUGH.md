# Start a new project from nothing

This walkthrough uses two complete files: a standing mission declares a durable project owner,
and a finite work mission assigns work to that owner. All names and data are invented. Use a
disposable ST3 installation with a working Codex login, from the repository root.

## Failure first: publication is not materialization

The tempting sequence is to publish `walkthrough-standing.kdl`, see no agent, and conclude that
there is no route from a definition to a worker. That conclusion confuses durable intent with a
running instance. Publication creates an immutable mission definition only. The first
`missions start` on that standing definition materializes its runtime. Until that happens, the
finite mission's assigned steps have no agent that can claim them.

The four-command sequence below is the supported way out. In particular, do not skip command 2.

Create an empty workspace and inspect both files before changing graph state:

```sh
walkthrough_workspace="$(mktemp -d)"
sed -n '1,200p' examples/st3/walkthrough-standing.kdl
sed -n '1,240p' examples/st3/walkthrough-work.kdl
```

For a longer mechanical gate, put its shell in a file and check that file with `/bin/bash -n`
before publication. This example's gate is a single command; its equivalent syntax-only check is:

```sh
/bin/bash -n -c '/usr/bin/test -s garden-note.md'
```

Then run these four ST3 commands in order.

1. Publish the standing definition:

   ```sh
   st3 missions publish examples/st3/walkthrough-standing.kdl --as person/operator
   ```

   Publication stores an immutable ready definition; it does not create an agent. At this point,
   `st3 missions ls --all` lists `mission/example/garden-owner` with zero runs.

2. Start the standing mission and materialize its agent:

   ```sh
   st3 missions start example/garden-owner --id example/garden-owner/standing \
     --workspace "$walkthrough_workspace" --as person/operator
   ```

   **This is the step that creates the agent.** Publishing the standing file alone never does.
   Starting it materializes `agent/example/garden-owner/standing/owner`; the agent remains
   available after finite work ends. Verify it with:

   ```sh
   st3 agents show agent/example/garden-owner/standing/owner
   ```

3. Publish the finite work definition:

   ```sh
   st3 missions publish examples/st3/walkthrough-work.kdl --as person/operator
   ```

   The file's publication-time handshake names the materialized owner. If command 2 was skipped,
   command 3 stops with an undeclared-recipient error instead of publishing work no agent can do.

4. Start the work and follow it:

   ```sh
   st3 missions start example/garden-work --id example/garden-work/first-change \
     --workspace "$walkthrough_workspace" --as person/operator --follow
   ```

   If command 3 did not publish the definition, this reports that the mission does not exist. The
   owner can now claim `write-note`; without command 2 that assigned work would be unclaimable.

`missions publish FILE --as ACTOR` previews and publishes exact authored KDL. A person actor is an
explicit trusted local operator. An agent actor is checked against the agent's already-current
`mission-authority`; authority written into the candidate being published cannot grant itself.

## Why the work mission has this shape

The `loop` owns the overall 30-minute budget. Each `round` creates fresh `write-note` work. `until`
checks the result after a round, `max-rounds` prevents an endless retry, and `on-exhausted` fails
with explicit human attention after the third miss. `${loop.feedback}` tells the next round which
until gate failed. Put corrective work inside `round`; put work that should happen once after a
successful loop, such as `report-completion`, after the loop and depend on the loop step.

Mechanical gates have a minimal `PATH` and no login shell. Use an absolute binary path, as the
example does with `/usr/bin/test`. Keep the owning step or loop timeout longer than the gate's
`time-limit`; here 30 minutes exceeds one minute. Syntax-check nontrivial shell with
`/bin/bash -n` before publishing it.

`report-completion` is a declared message step reached only after success. The `finally` block runs
for every terminal outcome and sends a second message that tells the operator to inspect the exact
status. Without those notifications, a run can finish correctly and still look abandoned.

After the run, `garden-note.md` is in the disposable workspace and the standing owner remains
running for the next mission. Remove the workspace when finished; immutable definitions and the
completed run remain in graph history by design.
