# Small Talk

Small Talk (`st3`) coordinates durable agent work across machines without losing operational truth.
Missions hold goals, constraints, ordered or parallel work, agents, terminals, gates, documents,
queues, nested missions, schedules, and cleanup. The graph remains authoritative across harness,
daemon, and machine restarts.

The normal product entry points answer three questions:

- `st3 now`: what is happening and what needs action?
- `st3 attention ls --as person/NAME`: what needs this person's decision?
- `st3 launch start ... --as person/NAME`: what work should the network launch next?

## Install

The Nix package installs `st3`, the shorter `st` symlink, the `stui` terminal
app, `st3-migrate`, and the pinned `pty` runtime:

```sh
nix profile install .#st3
st3 --help
```

For repository development:

```sh
cargo build -p st3 -p st3-migrate --locked
target/debug/st3 --help
```

Source builds need `pty` on `PATH`; the Nix package includes it. Native agents
also need their selected harness, such as Codex, Claude, pi, OMP, or OpenCode.

## Start and check this machine

```sh
st3 service install
st3 service status
st3 doctor --strict
st3 now
```

The default state directory is `${XDG_STATE_HOME:-$HOME/.local/state}/st3`. The local API uses a
Unix socket below `${XDG_RUNTIME_DIR}` when available and otherwise below the state directory.

The service manager and mission runtimes are separate. Restarting the daemon does not stop a live
agent, terminal, or exec process; reconciliation adopts the exact surviving incarnation. Linux uses
systemd user scopes and macOS uses independent process sessions. On macOS, run
`st3 service permissions` for the one-time Full Disk Access and Developer Tools instructions.

Finite client requests have bounded timeouts and actionable failures. Terminal attachment and an
explicit follow remain open after connecting. Fleet replication is optional; Fabric, Tailscale, or
another authenticated transport can expose the paired client and replication endpoints between
machines. See [fleet replication](docs/st3/replication.md).

## Launch work

`launch` turns a natural-language request into a durable planning conversation and an exact mission
preview. Human authority is always explicit as a complete `person/...` subject.

```sh
st3 launch start --id release request.md \
  --workspace /work/release \
  --as person/operator

st3 launch show launch/release/SESSION
st3 launch preview launch/release/SESSION --variant default
st3 launch approve-and-launch launch/release/SESSION PREVIEW_HASH \
  --workspace /work/release \
  --as person/operator
```

The planner can ask typed, revisioned questions. Answers may be boolean, single-choice,
multiple-choice, ranked-choice, or free text, and every answer may include an explanation. Use
`launch revise` to add feedback, `launch compare` to compare variants, and `launch cancel` to end an
unwanted conversation.

An approved mission can also be started separately:

```sh
st3 missions start release \
  --id release-candidate-42 \
  --input request="Prepare release 42" \
  --as person/operator \
  --follow
```

The mission run pins the exact revision, workspace, requester, and inputs. Missions permit one
nonterminal run by default unless their declaration opts into concurrent runs.

Exact hand-authored mission KDL does not need a planner session:

```sh
st3 missions publish missions/release.kdl --as person/operator
```

An agent can publish through the same command when its already-running declaration grants matching
`mission-authority publish`. Publishing and starting remain separate, explicit actions.

## Do mission work

Every native harness receives a generated `.st3/boot.md`. It lists the exact graph-owned work and
teaches the harness the small worker protocol:

```sh
"$ST3_BIN" work ls
"$ST3_BIN" work claim step-run/RUN/STEP
"$ST3_BIN" work progress step-run/RUN/STEP --summary "The tests now pass."
"$ST3_BIN" work complete step-run/RUN/STEP --summary "The change is ready."
```

Use `work release` when another eligible agent should take the step and `work fail` when its goal
cannot be met. An agent may publish a generated nested mission only from claimed work with declared
`mission-authority` and a matching `produces-mission` contract. Use `queue {}` in mission KDL when
source order is intentional; do not encode ordering only in prompts.

The versioned examples in [`examples/st3`](examples/st3/README.md) demonstrate queues, nested
missions, review/remediation, recurring work, observations, revisions, and bounded loops.

## Understand the network

These are the normal operational views:

```sh
st3 now
st3 machines
st3 missions ls
st3 missions show MISSION_OR_RUN
st3 agents ls
st3 agents tree
st3 work ls
st3 terminals ls
st3 activity
```

Current state is the default. Historical, stopped, and terminal records appear only behind the
command's explicit `--all` option. Bounded lists print the exact continuation command when another
page exists.

Use `st3 subject show SUBJECT` for one typed card, `st3 subject history SUBJECT` for its immutable
history, and `st3 schema subjects` to inspect the authoritative model. `st3 doctor` diagnoses local
health; `st3 repair dry-run` produces an exact bounded repair plan before any mutation.

## Attention and conversations

The human inbox contains only current gates, approvals, unread person messages, and explicit fault
requests for the selected person:

```sh
st3 attention ls --as person/operator
st3 attention show ATTENTION --as person/operator
```

Rendered items include exact approve, reject, read, resolve, or dismiss commands. No human action
inherits `ST_AGENT`, `ST_PERSON`, local configuration, or a parent terminal.

Conversations are normalized durable records:

```sh
st3 conversations send agent/fleet/example/worker \
  --from person/operator \
  --subject "Check the new request" \
  --body "A new mission step is ready."

st3 conversations ls agent/fleet/example/worker
st3 conversations read MESSAGE --as agent/fleet/example/worker
st3 conversations thread MESSAGE
```

`conversations sessions` and `conversations timeline` expose normalized Codex, Claude, and OMP
session history, including tool activity and available usage data.

## Import an existing harness session

`import` discovers native Codex, Claude, and OMP sessions without claiming they are already managed:

```sh
st3 import ls
st3 import ls --all
st3 import show SESSION
st3 import run SESSION --as person/operator
```

Saved legacy sessions are readable through the normalized conversation view. Import revalidates the
exact process fingerprint, stops only that process, publishes a durable resume mission, and starts
the same native session under st3 ownership. Ambiguous or changed processes are refused.

## Terminals

```sh
st3 terminals ls
st3 terminals peek PTY
st3 terminals attach PTY
st3 terminals send PTY "status"
```

`terminals attach` uses the same binary terminal transport as the runtime. Ctrl+\\ detaches without
stopping the session. The client restores terminal modes and sanitizes terminal state on normal
exit, remote EOF, and errors. Nested attachment is refused unless `--force` is intentional.

## Stable client data

Human output is readable by default. Add global `--json` for the stable client-v0 envelope used by
the future TUI and native applications:

```sh
st3 --json now
st3 --json machines
st3 --json conversations sessions
```

The client contract includes snapshots, fences, pagination, typed resources, structured diffs,
capabilities, events, pairing, and normalized session timelines. Raw document bytes, terminal
screens, streaming traces, and completion scripts retain their purpose-specific output.

## Documentation

- [Documentation index](docs/st3/README.md)
- [Architecture](docs/st3/design.md)
- [Mission graph runtime](docs/st3/mission-graph-runtime.md)
- [KDL lifecycle](docs/st3/kdl-lifecycle.md)
- [Schema registry](docs/st3/schema.md)
- [Examples](examples/st3/README.md)
- [Product roadmap](docs/st3/roadmap.md)

The st2 implementation remains in this repository for migration testing. It is not part of the st3
CLI contract.
