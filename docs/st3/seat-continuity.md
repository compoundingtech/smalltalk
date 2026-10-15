# Seat handoff

A seat's work lives in the graph, but a session's understanding does not. When a seat restarts and
the harness cannot continue the earlier conversation, the new session knows only what st gives it.
The handoff is that gift: one document per seat, written by the seat, and a first message that
puts it in front of the next session together with the live state of the seat's work.

Same-session resume (`claude --resume`, `codex resume`) stays preferred wherever it works. The
handoff covers the cases where it does not: a move to another account or model, a fresh session
after a declaration change, a seat killed without warning.

## The document

Each seat has one document, `doc/agents/IDENTITY/handoff`, where `IDENTITY` is the seat's name
without `agent/`. It is an ordinary versioned document, so every earlier handoff stays readable.

```sh
st agents handoff write --as "$ST_AGENT" <<'TEXT'
Reviewing the widget change (branch agent/widget, PR 12). Waiting on CI.
Next: address the review comment about retries, then ask the curator to queue it.
A watch on PR 12 is mine; nothing else is running.
TEXT
st agents handoff show
```

`write` replaces the handoff from standard input or a file. `show` prints the latest, for this seat
or any named seat.

Under `doc/agents/` a seat may bind exactly one name, its own `doc/agents/IDENTITY/handoff`. The
daemon refuses every other `doc/agents/…` name from a seat bound to an agent identity
(`document-not-yours`), including the seat's own other names, because the next session reads the
document as its own seat's words. The restriction applies to ancestry-bound seats only: a person's
or an unbound caller's writes, and every name outside `doc/agents/`, are unchanged.

A handoff carries what the graph does not: what the seat was in the middle of, the pull requests
and branches it owns, what it waits on, the next steps, and anything it started that still runs. A
step's progress notes cover the step; the handoff covers the seat. A seat killed without warning
leaves only what it wrote before, so it keeps the handoff current as it works.

## Before a planned restart

A planned restart is one st chooses to make while the seat is up: an operator's or agent's
`st agents restart`, a pool switch to another account at a limit, or a declaration change (an
account, model, workspace or launch change) applied to a running seat. Before st stops the seat it
sends it a "Prepare for restart" message from `daemon/runtime` with a deadline (two minutes), and
waits. The message carries the reserved tag `st3-restart-prepare:REQUEST`, so it wakes the seat.

The seat writes its handoff, which acknowledges the request, or runs `st agents handoff ready` when
the handoff it has is already current. St stops the seat at the acknowledgement or at the deadline,
whichever comes first, and records which: `runtime.reconcile-decision` claims with key
`restart-prepare` say `prepare`, then `ready` or `deadline-passed`. A handoff written before the
request does not acknowledge it. A readiness claim counts only when its recorded actor is the seat; as
with other claims the actor is what the caller asserted, so this keeps out accidents and other seats,
not a determined local caller. The handoff document is the stronger acknowledgement: the daemon
lets only the seat itself write it.

`st agents restart AGENT --now` skips the wait, for a seat that cannot answer (it records `operation: restart-now` on the request). Scripts that restart stub seats use it.

A seat that is not up and ready, such as one that has exited, is blocked at a prompt or is logged
out, has nobody to ask, so its restart goes ahead at once. A seat that stops answering (a pool seat
at its limit, for example) costs the deadline, not more. Crashes, `agents stop`, suspend and
mission `fresh-context` resets are not planned restarts and send nothing.

## What a new incarnation is sent

When a seat's new incarnation becomes ready, the daemon sends it one message from `daemon/runtime`,
"Session restarted: where you were". It holds

- the latest handoff, with its age and an immutable reference (a handoff over 16 KiB is cut and
  names the reference to read the rest);
- each unfinished step the seat holds or was working on, with its goals and last progress note (a ready step nobody has worked on is announced by its own ready-step message and is left out);
- the seat's unread mail, by sender and title.

The message carries the reserved tag `st3-session-start:INCARNATION`, so it always wakes the seat
and no seat can send one. It is written once per incarnation: the record is the message's own
idempotency key, so a daemon restart or a repeated pass sends nothing more.

A seat with no handoff and no unfinished steps is sent nothing. Neither is a seat's first
incarnation, which has no predecessor, nor one the daemon first finds running more than ten
minutes after it started (a daemon that was down, or the release that added this): such a session
already has its context, and an upgrade does not message every running seat. The daemon looks at
each incarnation once and writes only when it sends, so a settled fleet costs nothing per pass.

A session that continues the earlier conversation receives the message too. It says so: such a
session already knows what it describes.

## Tests

- `crates/st3/src/handoff.rs`: naming, write authority, and what the message renders.
- `tests/agents_restart.rs`: a requested restart waits for the handoff, goes ahead at the deadline,
  and a declaration change prepares first.
- `reconcile::tests::a_new_incarnation_is_first_sent_the_handoff_its_seat_left`: the first
  incarnation is told nothing, its replacement is told once, and an empty seat is told nothing.
