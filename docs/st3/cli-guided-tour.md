# st guided CLI tour

This is the runbook for the human walkthrough of the frozen `st 0.1.0` CLI. It covers every
public command and subcommand, but follows product workflows instead of alphabetical order. The
tour is observational by default: live fleet reads are safe, while mutations are inspected through
help and exercised only against disposable state.

## How we will run the tour

At the start, record the exact binary and repository revision:

```sh
command -v st
st --version
git -C ~/src/github.com/compoundingtech/st2--st3 rev-parse HEAD
st --help
```

For each numbered stop I will explain the command's job and why it exists. Alex will run the
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

This pass answers three questions: what is happening, what needs Alex, and how does Alex launch
new work?

### 1. `now` — one bounded operational answer

Why: this should be the first command a person runs, not a dashboard assembled from five other
commands.

```sh
st now --help
st now --as person/alex
st now --as person/alex --json
```

The default lists human attention only. Mission work is in `work ls` and Control; an explicit
`--owner-run` opts it into this combined view. Fleet diagnostics are in `doctor` and Operations.
Check whether the default is calm, current, and actionable. `--all`, `--owner-run`, `--cursor`, and
`--limit` must make sense from help alone.

### 2. `missions` — durable intent and execution

Why: a mission is the durable graph of goals and work; this surface owns publication, lifecycle,
progress, and usage.

```sh
st missions --help
st missions ls --help
st missions ls
st missions show --help
st missions show mission-run/64bcc9227e0166a571e09117d35c572e
st missions publish --help
st missions check --help
st missions start --help
st missions cancel --help
st missions outcome --help
st missions retire --help
```

`ls` and `show` are live reads. Publication, start, cancel, setting a finished run's outcome, and
retirement are reviewed through help here and are mutation-tested only in the disposable fixture
run.

### 3. `work` — the truthful queue and worker lifecycle

Why: people need an unfiltered view of current work; agents need explicit, fenced lease actions.

```sh
st work --help
st work ls --help
st work ls
st work ls --as agent/example/st3/standing/st3
st work show --help
st work show step-run/e3e841ba011236a21fe8bd3e50c21a1d/walkthrough-and-followup
```

Then inspect every lifecycle and revision action:

```sh
st work claim --help
st work renew --help
st work progress --help
st work complete --help
st work fail --help
st work release --help
st work wake --help
st work retry --help
st work publish-mission --help
st work revise --help
st work revision --help
st work revision show --help
st work revision generations --help
st work revision generation --help
st work revision approve --help
st work revision cancel --help
```

Check that a person can understand ownership, readiness, blockers, lease/incarnation, elapsed time,
goals, constraints, and usage without learning internal graph vocabulary.

### 4. `attention` — Alex's explicit inbox

Why: decisions and faults needing a person belong in one low-noise, actor-specific inbox.

```sh
st attention --help
st attention ls --help
st attention ls --as person/alex
st attention show --help
```

If `ls` returns an item, copy its ID into `show`. Inspect the mutation help without changing live
state:

```sh
st work ask --help
st work done --help
st attention approve --help
st attention reject --help
```

Every row should say why Alex is involved, what happens if he does nothing, whether it is stale,
and the exact actions available now.

A subscription that holds mission requests for a person raises one attention item. Inspect the
subscription request commands without changing live state:

```sh
st missions requests --help
st missions release --help
st missions cancel-request --help
```

### 5. `launch` — chat, shape, approve, and launch a mission

Why: `launch` is the human product workflow for turning intent into a durable mission. “Planning” is
an internal phase, not a competing public noun.

```sh
st launch --help
st launch ls --help
st launch ls
st launch show --help
st launch preview --help
```

If there is a current launch, use its ID for `show` and `preview`. Then inspect the complete
conversation lifecycle:

```sh
st launch start --help
st launch submit --help
st launch revise --help
st launch question --help
st launch answer --help
st launch compare --help
st launch propose --help
st launch approve --help
st launch approve-and-launch --help
st launch run --help
st launch cancel --help
```

Check that goals, constraints, graph structure, decisions and answer explanations, variants,
validation, diffs, risks, preview token, approval, and run state are visible without parsing KDL or
Markdown. Pay special attention to whether `approve`, `approve-and-launch`, and `run` are distinct
and understandable.

### 6. `conversations` — messages and normalized harness sessions

Why: human/agent mail and Codex, Claude, Pi, OMP, or OpenCode session timelines need one normalized product
surface.

```sh
st conversations --help
st conversations ls --help
st conversations ls person/alex
st conversations sessions --help
st conversations sessions
st conversations read --help
st conversations thread --help
st conversations timeline --help
```

`conversations ls --as MAILBOX` is the same as the positional mailbox, matching `read --as`. A
harness process with `ST_AGENT` can read, archive, send, reply, claim, and act on work only as that
agent; a different `agent/...` actor is refused.

Use a returned message ID for `read`/`thread` and a returned session ID for `timeline`. Inspect the
write paths:

```sh
st conversations send --help
st conversations reply --help
st conversations status --help
st conversations archive --help
st conversations export --help
```

Check role, ordering, partial/final state, tool calls and results, errors, token usage, reply
threading, read/archive state, redaction, truncation, and pagination.

Run one `send` twice: the second prints the same message ID, says on stderr that it was already
sent, and sends nothing (`already_sent: true` and the first `sent_at` with `--json`). A send that
times out prints its idempotency key and `st conversations status --idempotency-key KEY`, which
answers `landed` with the message's delivery or `not landed`; running the same send again is safe.

### 7. `agents` and `machines` — who is doing what, where

Why: the tree should make mission ownership visually obvious; the machine view should combine
reachability, health, and capacity.

```sh
st agents --help
st agents ls --help
st agents ls --status running --enrich
st agents tree --help
st agents tree --status running --enrich
st agents show --help
st agents queue --help
st agents queue move --help
st agents new --help
st agents new example --host HOST --print-kdl
st agents suspend --help
st agents resume --help
st machines --help
st machines
```

Copy an agent ID into `show`. Check tree nesting, durable seats versus mission-owned agents, current
runtime, host, work, conversation, stale state, and whether stopped history stays out of the default.
For a working agent, check that `show` names its current step and its last progress summary.
Check that `AUTHORITY` lists `fleet/PROJECT/*` with `(default)` for a durable seat a person
declared under `fleet/`, and `no missions` for a mission-owned agent.

Copy a durable seat into `agents queue`. Check that the current claim comes first, then each queued
mission run in order with `claimed`, `ready`, or `waiting`, and that `NEXT WORK` matches
`agents show`. `st missions queued AGENT` prints the exact same view for someone who thinks of
this as a mission question rather than an agent one. `agents queue move AGENT RUN --top`,
`--bottom`, `--before RUN`, or `--after RUN` is a person-authorized mutation; move only a run we
agreed to reorder, then confirm the move is listed with its author and time and that a held step
stayed held.

`agents suspend` is a mutation. Suspend only a seat we agreed to stop. Check that a busy seat is
refused with its reasons, that a quiet one shows `suspended` with its native session in `agents
show`, and that `agents resume` reports the same session.

`agents new --print-kdl` shows the seat declaration without applying it. Check that its workspace is
a new directory below that host's home and that the harness defaults match the fleet's existing
seats. Run it without `--print-kdl` only for an agent we agreed to start.

### 8. `terminals` — inspect and attach without shell nesting

Why: terminal access is a first-class cross-machine product capability with explicit view/control
authority and clean detach behavior.

```sh
st terminals --help
st terminals ls --help
st terminals ls
st terminals peek --help
st terminals attach --help
st terminals send --help
st terminals signal --help
```

Use a harmless live terminal for `peek`. Attach only when we have agreed which terminal; detach and
verify the caller's screen, cursor, input mode, and shell prompt are restored. Attach once to a
terminal on this host and once to one on another fleet host: the remote attach goes through the
client gateway as the configured person and should feel the same. `send` and `signal`
are control mutations and are not aimed at arbitrary live work.

### 9. `import` — adopt native Codex, Claude, Pi, OMP, and OpenCode sessions

Why: useful pre-st sessions should be readable before import and resumable under durable st
ownership afterward.

```sh
st import --help
st import ls --help
st import ls
st import show --help
st import run --help
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
st devices --help
st devices --as person/alex ls --help
st devices --as person/alex ls
st devices --as person/alex pair --help
st devices --as person/alex revoke --help
```

Pair and revoke only during the app/on-device proof. Check device name, scopes, expiry/revocation,
last activity, and whether the next action is obvious.

### 11. `activity` — resume live UI state

Why: every client needs bounded change delivery with a stable cursor and explicit full resync after
a gap.

```sh
st activity --help
st activity --limit 10
st activity --limit 10 --json
```

We will briefly try `--follow`, interrupt it, and verify the resume cursor. Check that the feed is
useful to a human while remaining an adequate TUI/mobile synchronization primitive.

## Pass 2: operator recovery

### 12. `doctor` and `repair` — diagnose first, repair by exact plan

Why: `doctor` identifies actionable faults; `repair` is a bounded, preview-token-authorized way to
converge known contradictions without rewriting history.

```sh
st doctor --help
st doctor
st doctor --strict
st repair --help
st repair dry-run --help
st repair dry-run
st repair apply --help
```

Apply nothing unless the dry-run reports a real, understood plan. Check pass/warn/fail semantics,
exit status, evidence, remediation, stable repair classes, approval token, and zero-change retry.

### 13. `replication` — prove fleet convergence

Why: transport reachability and immutable-record convergence need explicit diagnostics rather than
being mistaken for mission health.

```sh
st replication --help
st replication status --help
st replication status
st replication invalid --help
st replication invalid
st replication inspect --help
st replication diff --help
st replication repair --help
```

Run `inspect` or `diff` only when `status` supplies a concrete record or peer. `repair` is a mutation
reviewed through help unless a known invalid record has an approved replacement.

### 14. `service` and `up` — own the local daemon lifecycle

Why: daemon installation, supervision, configuration, restart, and permissions are one local
operator workflow.

```sh
st service --help
st service status --help
st service status
st service permissions --help
st service install --help
st service restart --help
st service uninstall --help
st service reset --help
st up --help
```

Do not run install, restart, uninstall, reset, or a second foreground daemon during the live tour.
`reset` must look unmistakably destructive. Check whether service status explains the installed
binary, config, sockets, processes, and logs needed for recovery.

## Pass 3: expert and agent tools

### 15. `subject` and `trace` — bounded graph explanation

Why: experts need one typed subject explorer and immutable history without leaking storage tables
into normal product commands.

```sh
st subject --help
st subject show --help
st subject show agent/example/st3/standing/st3
st subject history --help
st subject history agent/example/st3/standing/st3 --limit 20
st trace --help
st trace show --help
st trace show agent/example/st3/standing/st3 --limit 20
st trace wait --help
```

We do not leave a wait running. Check the boundary between product detail (`agents show`) and expert
history (`subject`/`trace`).

### 16. `schema` and `claim` — discover and use the graph vocabulary

Why: registered types and write policy must be inspectable; low-level observation remains explicit
and expert-only.

```sh
st schema --help
st schema subjects --help
st schema subjects
st schema resources --help
st schema resources
st schema claims --help
st schema claims
st schema show --help
st schema export --help
st claim --help
```

Use one returned claim kind for `schema show`. Do not publish a live low-level claim merely to demo
the parser. Check that every name has purpose, fields, version, authority, and evidence requirements.

### 17. `documents` — immutable exact-byte evidence

Why: missions and reviews need content-addressed artifacts larger than CLI prose.

```sh
st documents --help
st documents ls --help
st documents ls
st documents get --help
st documents put --help
```

Use an existing reference for `get`; exercise `put` only in disposable state. Check hash visibility,
version history, exact-byte retrieval, and safe output behavior.

### 17a. `blobs` — images a message carries

Why: a screenshot must reach a seat on another machine without entering sync.

```sh
st blobs --help
st blobs put --help
st blobs put screenshot.png --as person/NAME
st blobs get --help
st conversations send --help
```

`put` keeps a PNG, JPEG, GIF or WebP image of at most 10 MiB on this member and prints
`blob/<sha256>`. `conversations send --attach FILE` uploads and attaches in one step, and
`conversations read` lists each attachment with the `blobs get` command that reads it. Exercise
`get` with the `--message` that carries the image. Check that an image over 10 MiB, a text file
named `.png`, and a fifth attachment are refused in words.

### 18. `diagnostic` — an agent's authorized harness-failure path

Why: a worker needs one typed way to report that its own harness failed; ordinary conversation text
must not impersonate an operational fault.

```sh
st diagnostic --help
```

This is agent-only and mutating, so the live human tour reviews help and the already automated
failure tests rather than publishing a fake fault.

### 18a. `gh` — a seat's watch on a GitHub issue or pull request

Why: a seat that asked a question on GitHub, or opened a pull request, needs to hear the answer and
the checks without polling. A watch wakes it once for each comment or review by anyone else, each
time the required checks on the head turn pass or fail, and when the thread closes or merges.
The watch survives stop/start, suspend/resume and daemon restarts; wakes wait as mail while the
seat is stopped. Retirement or an explicit fresh conversation ends it silently, and the deadline
sends a final wake. The shared observer keeps polling until the last watch ends.

```sh
st gh --help
st gh watch --help
st gh ls --all
```

`comment` posts as the seat and records the new comment's GitHub ID as its own, so the seat's
watches skip it; `own URL` records one posted some other way. `watch`, `unwatch`, `comment` and
`own` are agent-only and mutating, and they reach GitHub, so the live human tour reviews help and
`ls --all`; the automated watch tests cover the rest.

### 19. `skill` — how an agent seat uses st

Why: an agent seat starts with no prompt, so the only st text an agent sees is the skill its driver
installs. It must match the binary that serves the commands it names.

```sh
st skill
st skill install --help
```

Check that the skill describes st without rules of conduct, that its description applies only when
`ST_AGENT` is set, and that `install` names each harness directory it writes.

### 20. `completions` — shell discoverability

Why: completion keeps the large but intentional command surface navigable, and offers the live
terminals, agents, missions, and other entities an argument accepts, each with a description
([spec](cli-completion/spec.md)).

```sh
st completions --help
st completions zsh >/dev/null
COMPLETE=fish st -- st terminals attach ''
st terminals attach steward
```

Check that each stub is printed, that entity candidates carry descriptions, that hidden/internal
commands remain hidden, and that an ambiguous short name lists its matches.

## Exit criteria

The walkthrough is complete when every public path above has a recorded disposition, every claimed
bug has reproducible evidence, and the entire agreed finding set is encoded in one autonomous
follow-up mission. The walkthrough itself does not rename the repository or begin TUI/iOS
implementation.
