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

## Driver retries during a daemon outage

Resumed native drivers retain the deploy settle before control reports and work renewals become
eligible. Delivery and local housekeeping keep their regular interval during that settle, but
they do not poll the deferred control retry or update its success/failure state. Renewals consume
neither their interval nor their retry tick before eligibility. The first eligible loop iteration
may perform one due attempt; skipped interval ticks do not become a catch-up burst. Settle itself
never resets a failure count or retry deadline. After eligibility, exact retry deadlines resume
waking between ordinary interval ticks. Claude attachment/readiness and other settled guards
retain the same eligibility boundary.

The native driver keeps the provider alive when the daemon cannot answer. Mailbox binding,
mailbox opening, startup calls, periodic control reports and work renewal keep separate retry
state. Repeated failures use base waits of 1, 2, 4, 8, 16 and then 30 seconds. The random share
is between half and one and a half of the base, with a hard 30-second cap. The first successful
operation clears its failure count. A valid mailbox frame counts as a successful reconnect,
not just opening a socket.

Mailbox transport establishment means `Client::open_mailbox` returned after the WebSocket HTTP 101
upgrade, before any application frame. Open and handshake failures use the exponential curve;
after an accepted upgrade, report-write errors, EOF and stream errors retain the original fixed
one-second base with 0.5–1.5-second jitter. The upgrade does not reset the open-failure count:
after two open failures, an upgraded stream that fails before a decoded frame leaves the next
open failure at base four seconds. Reading a decoded application frame resets it, making the
next open failure start at base one. A server that accepts the upgrade and then drops every
connection is therefore retried at the fixed jittered one-second cadence forever, as before
this change. The exponential policy does not protect or repair that case.

A refused connection or a missing socket stays at the one-second restart base instead of
growing. Its random wait is 0.5 to 1 second, so the retry wait itself does not delay discovery
of a restarted daemon by more than one second. Request execution time is separate from this wait.
The native session report, binding watch and harness publications inside the gated control tick
back off together. After repeated failures, the whole block can wait up to 30 seconds plus request
time. Delivery-control work and Claude attachment checks outside that block keep their cadence,
as does provider-completion handling. This is deliberate degradation while the daemon is slow;
settle gating of these ticks is a separate change. A fast typed API refusal from a live daemon
also grows that operation's failure count, just as a timeout does; it is not a refused socket.
A caller's existing terminal-refusal handling still applies before another attempt is scheduled.
Work renewal has its own retry state; control-report success cannot clear a failed renewal's
backoff. Renewal writes keep their
existing per-step, per-actor, per-minute idempotency keys. Warning logs remain rate limited.
Each known held-work lease also caps the retry wait at half of its remaining margin. A retry
keeps the last known expiry if the next work read fails; an expired lease gets no 30-second wait.
An authoritative read with no held work clears that old lease bound, so an idle driver's later
timeout retries can grow again. A held claim with an unknown expiry does not clear it.
The normal lease is ten minutes and renewal starts once a minute. No retry policy can preserve
a claim through an outage longer than its lease.

This reduces repeated requests when the daemon is slow; it does not fix the cause of a slow
request. A failure may now delay an update by up to 30 seconds, plus request time. List reads,
stream events, stored records and the provider's own retry policy do not change.

## Tests

- `crates/st3-reconcile/src/handoff.rs`: naming, write authority, and what the message renders.
- `tests/agents_restart.rs`: a requested restart waits for the handoff, goes ahead at the deadline,
  and a declaration change prepares first.
- `reconcile::tests::a_new_incarnation_is_first_sent_the_handoff_its_seat_left`: the first
  incarnation is told nothing, its replacement is told once, and an empty seat is told nothing.
