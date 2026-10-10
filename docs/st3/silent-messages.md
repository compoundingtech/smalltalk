# Silent and wake messages

A message has one of two kinds: `silent` or `wake`. Every sender and seat defaults to wake.
Silent mail is stored and readable, but wakes nobody; the recipient sees a capped batch at its
next real turn or reads it on demand. FYI is one use of silent, rather than a separate type.

```sh
st conversations send agent/example/builder -m "Merged #12." --kind silent --as agent/example/reviewer
st conversations reply message/ID -m "Thanks, noted." --kind silent --as agent/example/builder
st conversations send agent/example/builder -m "Which branch?" --kind wake --as agent/example/reviewer
```

The client API's `message.send` takes `kind: "silent" | "wake"`; omission means wake.
There is no per-seat wake policy, question tag, question-thread inference or content classifier.
A reply independently defaults to wake, including an answer to a silent message. To keep a
reply silent, its sender explicitly chooses silent. Status belongs in `work progress` (no wake),
run events in the mission's `report-to`, and questions, answers and handoffs in wake messages
or `work handoff`. Other informational messages use silent.

These always wake even if marked silent:

- person messages, including imported external messages;
- ready steps, faults, GitHub watch events and work handoffs;
- daemon run reports, provider-capacity retry nudges, product-wait and planning notices.

The accepted silent kind is recorded as `st3-silent` in message tags. Signed silent requests
carry that tag in the device's signed tags; signed claims are never rewritten. Message views
retain the accepted tag; person and system exceptions still always wake. The raw tag-based
conversation endpoint remains available. Reserved daemon event tags are a cooperative cost
filter, not authentication: the raw endpoint trusts its `from` identity. Generic agent `launch`
tags remain allowed and do not bypass silence; daemon planning `launch` notices always wake.

## How it works

The daemon decides once at acceptance, without a recipient-policy query or question-thread state.
Readers classify the stored kind from tags and sender; delivery adds no per-message query.
Retries preserve canonical accepted input and reject a changed kind.

A seat's mailbox stream keeps held mail admitted, but it leaves unoffered held mail out of the
frame it sends until a message that wakes the seat is in it. At most the eight newest unoffered silent messages go out with that wake, in send order.
A line reports `K older silent held; st conversations ls`; the remaining mail stays held for
on-demand reads or a later wake. Each harness shows a held message with
`(silent: held without waking you until this turn)`. Once offered, held mail follows the ordinary
staged, delivered and read receipts. Reconnects load bodies for at most eight held subjects, regardless of age; all others remain durable. A changed-claim update coalesces waking messages into one held-batch refill. The existing delivery query still sorts mailbox metadata, so this bounds held body hydration rather than total mailbox work. Remaining-count notices can stay stale until the next wake after an on-demand read. Native harnesses also forward `dictated` on person messages so their existing dictated notice is visible.

Held mail is never lost. Until it is read it is unread like any other message:
`st conversations ls` lists it, and `st conversations read` reads it on demand. Held mail waiting
for a turn does not count as late in the `message-delivery` check, does not keep a stopping seat
running, and does not block a rollout drain. `st doctor`'s `held-mail` check lists each seat with
up to 128 oldest unoffered silent messages unread for over a day, selected from an indexed held flag. Backlog cleanup excludes unoffered silent messages.

## Coordination cost

`st usage --hours 24 --messages-only --json` reads counts without message bodies or token
usage: `agent_to_agent`, its held `silent` subset, and `to_person`. Each message subject counts
once, including messages already read. Daemon and person senders, delivery probes, soaks and explicit synthetic test traffic do not count. The covering
metadata index is maintained by writers; historical bootstrap advances at most eight subjects per
writer batch, stopping between subjects once it has spent ten milliseconds. One daemon task
awaits each bootstrap job on the existing FIFO writer, pauses 100 milliseconds between jobs, and stops
when complete; per-claim projections cannot multiply the historical budget. `complete: false` means bootstrap is unfinished, so collectors publish no ratio.
Reads never advance bootstrap. The factory SLO timer divides each count by merges in the
rolling day and publishes informational values; zero merges gives no ratio.

## Upgrades and recovery

Old daemons silently ignore the typed `kind` field and do not hold `st3-silent` tags, so a
silent message may wake its recipient. Old CLIs reject `--kind`. The CLI warns when an agent's
requested silent tag is absent from the receipt; SDK callers must inspect returned tags in a
mixed fleet. Upgrade all senders and owner daemons before relying on silent delivery.
Downgrading below this build is unsupported; use forward recovery. Earlier FYI/silent preview
binaries have incompatible local metadata and positional inserts and may fail sends or startup.
Pre-feature binaries lose no-wake guarantees. Rust struct literals must supply `kind: MessageKind`;
deserialization and CLI defaults are wake. There is no seat declaration or restart change.

Bootstrap reports `cursor`, `ceiling`, `progress_ms` and `complete` from local metadata;
doctor warns if unfinished bootstrap makes no progress for 15 minutes. On reopening after
an old binary ran, the writer reopens the uncovered send tail in bounded jobs. Corrupt source
claims are not skipped: bootstrap fails closed and keeps counts incomplete so a collector
cannot publish a plausible undercount. A failed startup bootstrap page rolls back and logs the error while allowing the daemon to start; later bootstrap remains incomplete. Repair the source using existing recovery procedures.

At eight subjects per job with a 100-millisecond pause, 100,000 historical sends need at
least about 20.8 minutes and one million about 3.5 hours, plus writer contention. Until
bootstrap completes, ratios are unavailable, held-mail diagnostics are partial, and the
backlog banner is suppressed to avoid reporting held mail as late. Separating held-only
bootstrap and displaying a pending-exclusion banner are deferred. Held counts use the canonical earliest send timestamp; a replicated duplicate's later unread
timestamp can make the cosmetic backlog count conservative. Cleanup revalidates each message
and still preserves unoffered silent messages. Optimizing the existing metadata sort and refreshing
remaining notices after on-demand reads are deferred.
