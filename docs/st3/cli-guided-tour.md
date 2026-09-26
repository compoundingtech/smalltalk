# st3 guided CLI tour

This is the runbook for the human walkthrough of the frozen `st3 0.1.0` CLI. It covers every
public command and subcommand, but follows product workflows instead of alphabetical order. The
tour is observational by default: live fleet reads are safe, while mutations are inspected through
help and exercised only against disposable state.

## How we will run the tour

At the start, record the exact binary and repository revision:

```sh
command -v st3
st3 --version
git -C ~/src/github.com/compoundingtech/st2--st3 rev-parse HEAD
st3 --help
```

For each numbered stop I will explain the command's job and why it exists. Nathan will run the
root or subcommand help, run the safe example, and report anything surprising. We record each
finding before moving on:

| Field | Meaning |
|---|---|
| Path | Exact command or subcommand |
| Type | bug, naming, help, output, workflow, missing capability, or delight |
| Severity | blocking, confusing, rough edge, or polish |
| Evidence | Command, output excerpt, and subject ID when applicable |
| Desired behavior | What a person or agent expected instead |

For every read screen, check the same six things: the default answers the obvious question; the
header and counts agree with the rows; IDs can be copied into the corresponding detail command;
empty state is clear; `--json` carries the same membership and meaning; pagination and `--all` are
discoverable rather than surprising.

## Pass 1: the everyday product loop

This pass answers three questions: what is happening, what needs Nathan, and how does Nathan launch
new work?

### 1. `now` — one bounded operational answer

Why: this should be the first command a person runs, not a dashboard assembled from five other
commands.

```sh
st3 now --help
st3 now --as person/nathan
st3 now --as person/nathan --json
```

The default lists human attention only. Mission work is in `work ls` and Control; an explicit
`--owner-run` opts it into this combined view. Fleet diagnostics are in `doctor` and Operations.
Check whether the default is calm, current, and actionable. `--all`, `--owner-run`, `--cursor`, and
`--limit` must make sense from help alone.

### 2. `missions` — durable intent and execution

Why: a mission is the durable graph of goals and work; this surface owns publication, lifecycle,
progress, and usage.

```sh
st3 missions --help
st3 missions ls --help
st3 missions ls
st3 missions show --help
st3 missions show mission-run/64bcc9227e0166a571e09117d35c572e
st3 missions publish --help
st3 missions start --help
st3 missions cancel --help
```

`ls` and `show` are live reads. Publication, start, and cancel are reviewed through help here and
are mutation-tested only in the disposable fixture run.

### 3. `work` — the truthful queue and worker lifecycle

Why: people need an unfiltered view of current work; agents need explicit, fenced lease actions.

```sh
st3 work --help
st3 work ls --help
st3 work ls
st3 work ls --as agent/fleet/st3/standing/st3
st3 work show --help
st3 work show step-run/e3e841ba011236a21fe8bd3e50c21a1d/walkthrough-and-followup
```

Then inspect every lifecycle and revision action:

```sh
st3 work claim --help
st3 work renew --help
st3 work progress --help
st3 work complete --help
st3 work fail --help
st3 work release --help
st3 work wake --help
st3 work publish-mission --help
st3 work revise --help
st3 work revision --help
st3 work revision show --help
st3 work revision generations --help
st3 work revision generation --help
st3 work revision approve --help
st3 work revision cancel --help
```

Check that a person can understand ownership, readiness, blockers, lease/incarnation, elapsed time,
goals, constraints, and usage without learning internal graph vocabulary.

### 4. `attention` — Nathan's explicit inbox

Why: decisions and faults needing a person belong in one low-noise, actor-specific inbox.

```sh
st3 attention --help
st3 attention ls --help
st3 attention ls --as person/nathan
st3 attention show --help
```

If `ls` returns an item, copy its ID into `show`. Inspect the mutation help without changing live
state:

```sh
st3 attention request --help
st3 attention resolve --help
st3 attention approve --help
st3 attention reject --help
```

Every row should say why Nathan is involved, what happens if he does nothing, whether it is stale,
and the exact actions available now.

### 5. `launch` — chat, shape, approve, and launch a mission

Why: `launch` is the human product workflow for turning intent into a durable mission. “Planning” is
an internal phase, not a competing public noun.

```sh
st3 launch --help
st3 launch ls --help
st3 launch ls
st3 launch show --help
st3 launch preview --help
```

If there is a current launch, use its ID for `show` and `preview`. Then inspect the complete
conversation lifecycle:

```sh
st3 launch start --help
st3 launch submit --help
st3 launch revise --help
st3 launch question --help
st3 launch answer --help
st3 launch compare --help
st3 launch propose --help
st3 launch approve --help
st3 launch approve-and-launch --help
st3 launch run --help
st3 launch cancel --help
```

Check that goals, constraints, graph structure, decisions and answer explanations, variants,
validation, diffs, risks, preview token, approval, and run state are visible without parsing KDL or
Markdown. Pay special attention to whether `approve`, `approve-and-launch`, and `run` are distinct
and understandable.

### 6. `conversations` — messages and normalized harness sessions

Why: human/agent mail and Codex, Claude, Pi, OMP, or OpenCode session timelines need one normalized product
surface.

```sh
st3 conversations --help
st3 conversations ls --help
st3 conversations ls person/nathan
st3 conversations sessions --help
st3 conversations sessions
st3 conversations read --help
st3 conversations thread --help
st3 conversations timeline --help
```

Use a returned message ID for `read`/`thread` and a returned session ID for `timeline`. Inspect the
write paths:

```sh
st3 conversations send --help
st3 conversations reply --help
st3 conversations archive --help
st3 conversations export --help
```

Check role, ordering, partial/final state, tool calls and results, errors, token usage, reply
threading, read/archive state, redaction, truncation, and pagination.

### 7. `agents` and `machines` — who is doing what, where

Why: the tree should make mission ownership visually obvious; the machine view should combine
reachability, health, and capacity.

```sh
st3 agents --help
st3 agents ls --help
st3 agents ls --status running --enrich
st3 agents tree --help
st3 agents tree --status running --enrich
st3 agents show --help
st3 machines --help
st3 machines
```

Copy an agent ID into `show`. Check tree nesting, durable seats versus mission-owned agents, current
runtime, host, work, conversation, stale state, and whether stopped history stays out of the default.

### 8. `terminals` — inspect and attach without shell nesting

Why: terminal access is a first-class cross-machine product capability with explicit view/control
authority and clean detach behavior.

```sh
st3 terminals --help
st3 terminals ls --help
st3 terminals ls
st3 terminals peek --help
st3 terminals attach --help
st3 terminals send --help
st3 terminals signal --help
```

Use a harmless live terminal for `peek`. Attach only when we have agreed which terminal; detach and
verify the caller's screen, cursor, input mode, and shell prompt are restored. `send` and `signal`
are control mutations and are not aimed at arbitrary live work.

### 9. `import` — adopt native Codex, Claude, Pi, OMP, and OpenCode sessions

Why: useful pre-st3 sessions should be readable before import and resumable under durable st3
ownership afterward.

```sh
st3 import --help
st3 import ls --help
st3 import ls
st3 import show --help
st3 import run --help
```

Use a returned session ID for `show`; do not run an import unless we selected a disposable or
intentionally adoptable session. Check harness type, live/saved state, workspace, process fence,
importability, normalized timeline link, and refusal reason.

An import performs an exact fenced takeover: it stops only the process whose native session and
start fingerprint still match, declares one durable ownerless agent seat, records the native
session identity in the graph, and starts that seat with the harness's canonical resume arguments.

### 10. `devices` — pair the TUI/mobile trust boundary

Why: remote authority belongs to a named, revocable device/person pairing, never to an actor string
supplied by an untrusted request.

```sh
st3 devices --help
st3 devices --as person/nathan ls --help
st3 devices --as person/nathan ls
st3 devices --as person/nathan pair --help
st3 devices --as person/nathan revoke --help
```

Pair and revoke only during the app/on-device proof. Check device name, scopes, expiry/revocation,
last activity, and whether the next action is obvious.

### 11. `activity` — resume live UI state

Why: every client needs bounded change delivery with a stable cursor and explicit full resync after
a gap.

```sh
st3 activity --help
st3 activity --limit 10
st3 activity --limit 10 --json
```

We will briefly try `--follow`, interrupt it, and verify the resume cursor. Check that the feed is
useful to a human while remaining an adequate TUI/mobile synchronization primitive.

## Pass 2: operator recovery

### 12. `doctor` and `repair` — diagnose first, repair by exact plan

Why: `doctor` identifies actionable faults; `repair` is a bounded, preview-token-authorized way to
converge known contradictions without rewriting history.

```sh
st3 doctor --help
st3 doctor
st3 doctor --strict
st3 repair --help
st3 repair dry-run --help
st3 repair dry-run
st3 repair apply --help
```

Apply nothing unless the dry-run reports a real, understood plan. Check pass/warn/fail semantics,
exit status, evidence, remediation, stable repair classes, approval token, and zero-change retry.

### 13. `replication` — prove fleet convergence

Why: transport reachability and immutable-record convergence need explicit diagnostics rather than
being mistaken for mission health.

```sh
st3 replication --help
st3 replication status --help
st3 replication status
st3 replication invalid --help
st3 replication invalid
st3 replication inspect --help
st3 replication diff --help
st3 replication repair --help
```

Run `inspect` or `diff` only when `status` supplies a concrete record or peer. `repair` is a mutation
reviewed through help unless a known invalid record has an approved replacement.

### 14. `service` and `up` — own the local daemon lifecycle

Why: daemon installation, supervision, configuration, restart, and permissions are one local
operator workflow.

```sh
st3 service --help
st3 service status --help
st3 service status
st3 service permissions --help
st3 service install --help
st3 service restart --help
st3 service uninstall --help
st3 service reset --help
st3 up --help
```

Do not run install, restart, uninstall, reset, or a second foreground daemon during the live tour.
`reset` must look unmistakably destructive. Check whether service status explains the installed
binary, config, sockets, processes, and logs needed for recovery.

## Pass 3: expert and agent tools

### 15. `subject` and `trace` — bounded graph explanation

Why: experts need one typed subject explorer and immutable history without leaking storage tables
into normal product commands.

```sh
st3 subject --help
st3 subject show --help
st3 subject show agent/fleet/st3/standing/st3
st3 subject history --help
st3 subject history agent/fleet/st3/standing/st3 --limit 20
st3 trace --help
st3 trace show --help
st3 trace show agent/fleet/st3/standing/st3 --limit 20
st3 trace wait --help
```

We do not leave a wait running. Check the boundary between product detail (`agents show`) and expert
history (`subject`/`trace`).

### 16. `schema` and `claim` — discover and use the graph vocabulary

Why: registered types and write policy must be inspectable; low-level observation remains explicit
and expert-only.

```sh
st3 schema --help
st3 schema subjects --help
st3 schema subjects
st3 schema resources --help
st3 schema resources
st3 schema claims --help
st3 schema claims
st3 schema show --help
st3 schema export --help
st3 claim --help
```

Use one returned claim kind for `schema show`. Do not publish a live low-level claim merely to demo
the parser. Check that every name has purpose, fields, version, authority, and evidence requirements.

### 17. `documents` — immutable exact-byte evidence

Why: missions and reviews need content-addressed artifacts larger than CLI prose.

```sh
st3 documents --help
st3 documents ls --help
st3 documents ls
st3 documents get --help
st3 documents put --help
```

Use an existing reference for `get`; exercise `put` only in disposable state. Check hash visibility,
version history, exact-byte retrieval, and safe output behavior.

### 18. `diagnostic` — an agent's authorized harness-failure path

Why: a worker needs one typed way to report that its own harness failed; ordinary conversation text
must not impersonate an operational fault.

```sh
st3 diagnostic --help
```

This is agent-only and mutating, so the live human tour reviews help and the already automated
failure tests rather than publishing a fake fault.

### 19. `completions` — shell discoverability

Why: generated completion keeps the large but intentional command surface navigable.

```sh
st3 completions --help
st3 completions bash >/dev/null
st3 completions zsh >/dev/null
st3 completions fish >/dev/null
```

Check generation, installation guidance, and whether hidden/internal commands remain hidden.

## Exit criteria

The walkthrough is complete when every public path above has a recorded disposition, every claimed
bug has reproducible evidence, and the entire agreed finding set is encoded in one autonomous
follow-up mission. The walkthrough itself does not rename the repository or begin TUI/iOS
implementation.
