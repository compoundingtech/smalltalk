# st3 agent migration

Status: current isolated migration guide.

This guide moves one agent from st2 to st3. It does not import st2 history or share a live catalog.

Do not use this guide as authority to change a live agent. A person must select each agent and authorize its cutover.

## Source repository

Keep st3 declarations in a private repository that is separate from the st2 catalog.

Store mission KDL, host KDL, redacted host documents, and migration notes there. Do not store the claims database or runtime workspaces.

Do not store credentials, raw private measurements, message history, or personal data in the repository.

## Prepare one host document

Write stable facts that an agent on the host needs. Do not put current work in this document.

Store the bytes first:

```sh
st3 documents put hosts/local.md --as doc/hosts/local
```

The command returns the exact SHA-256 reference. Put that reference in a host declaration:

```kdl
version 2

host "local" {
  document "doc/hosts/local@SHA256"
}
```

Include the host declaration in the mission candidate. Approval fails when the exact document is
absent.

## Prepare a durable seat

Use a top-level agent when the harness must remain available across finite missions.

```kdl
version 2

agent "agents/example" {
  workspace "/work/example"
  harness "codex" {}
  restart "always"
}
```

The stable subject is `agent/agents/example`. The host places the seat but does not become part of
that slash-qualified identity.

The harness prompt is optional. st3 generates `.st3/boot.md` and appends the required boot instruction.

Put current work in separate finite mission steps assigned to this subject. Do not put it in the
boot file, host document, or harness prompt.

Publish exact authored seat KDL without creating a planner session:

```sh
st3 agents apply agents/example.kdl --as person/operator
```

The convenience form is `st3 agents start agents/example --harness codex --workspace
/work/example --as person/operator`. Add `--print-kdl` to review its exact KDL first.

## Review before start

Create and review a launch before any cutover:

```sh
st3 launch start --id agents/example migration-request.md \
  --workspace /work/example \
  --as person/operator
st3 launch show launch/agents/example/SESSION
st3 launch preview launch/agents/example/SESSION
```

Inspect the candidate KDL, subject diff, and document hashes. Approve the exact preview separately;
approval does not start the mission.

## Rehearse in isolation

Run the `agent-migration-rehearsal` eval before a live cutover. The eval uses a separate run workspace and no st2 state.

The rehearsal proves these facts:

- the exact host document exists;
- the runtime renders the canonical boot file;
- an agent claims and completes normal graph work;
- mission cleanup stops the test agent.

## Start the st3 seat and one work mission

Do not start the live st3 seat while st2 still owns the same agent workspace.

Stop the authorized st2 agent first. Remove only the st2-generated files that conflict with the st3 render.

Apply the seat, then use a readable run ID for the first finite work trial:

```sh
st3 agents apply agents/example.kdl --as person/operator
st3 missions start work/example \
  --id agents/example/pilot \
  --workspace /work/st3-runs/example \
  --as person/operator \
  --follow
```

Verify the exact mission run, agent subject, runtime incarnation, generated boot file, host document, and work queue.

The agent subject remains `agent/agents/example` across that mission and every later mission.

## Cut over the live identity

Stop the old st2 agent only after the authorized operator accepts the isolated st3 proof.

Use the st2 supervision mechanism for the old agent. Do not remove its history or catalog files during the trial.

Verify the old process is stopped. Keep the verified st3 run active, and verify its process arguments and graph state again.

Run both systems during the fleet migration. Do not bridge their messages or copy old inbox state.

## Revise or cancel

Publish or start a finite mission when the seat needs new work. A mission revision creates a
successor run generation without changing the seat identity.

Publish an exact root stop to stop the st3 seat:

```kdl
version 2

stop "agent/agents/example"
```

Cancel a still-running work mission separately. Omission never cancels a run or stops a seat.
Deleting a local KDL file never changes graph state.

## Fleet sequence

Move one low-risk agent first. Keep each successful agent on st3 while the next agent moves.

For each agent, repeat the same preview, isolated proof, start, verification, and old-runtime stop sequence.

Do not create a root, supervisor, or chief-of-staff runtime for host control. The st3 daemon owns runtime supervision.
