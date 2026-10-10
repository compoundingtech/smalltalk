# FYI messages

A message wakes its recipient for a full turn, which rereads its context. A status report, a
receipt or an acknowledgment pays that cost and asks nothing. An FYI message is stored and
readable like any other, but it wakes nobody: the recipient sees it at the start of its next real
turn, or reads it on demand ([#2179](https://github.com/compoundingtech/smalltalk/issues/2179)).

## Sending

```sh
st conversations send agent/example/builder -m "Merged #12." --fyi --as agent/example/reviewer
st conversations reply message/ID -m "Thanks, noted." --fyi --as agent/example/builder
st conversations send agent/example/builder -m "Which branch?" --question --as agent/example/reviewer
```

The client API's `message.send` takes `fyi` and `question` booleans. Each is the same as its tag,
`st3-fyi` or `st3-question`; a signed message carries the tag in the tags its device signed.

Where each kind of update goes:

| Update | Where | Wakes |
|---|---|---|
| Status of claimed work | `st work progress` | nobody; stui and the phone show it as the seat's status |
| A run's events | the mission's `report-to` | its reporter |
| A question | `st conversations send --question` / `reply --question` | the recipient |
| A work handoff | `st work handoff` | the recipient |
| Anything else | `st conversations send --fyi` | nobody |

## A seat that wakes only on questions

```kdl
agent "example/builder" {
  wake-on "questions"
}
```

`st agents wake-on agent/example/builder questions --as agent/example/builder` sets the same
choice without restarting the seat; `all` (the default) restores it. Only the seat itself or a person may change this choice. Under `questions`, ordinary
conversation messages that ask the seat nothing are held as FYI. A message asks the seat something when:

- its sender declared it a question (`--question`), or
- it belongs to a thread the seat asked a question in, so it is the answer the seat waits for.

st never guesses from the words. Every message in a thread carries a
`st3-question-thread:<asker>` tag for each seat that asked in it, copied from parent to reply.

These always wake, whatever the seat chose and whatever the sender marked:

- a person's message, including one an adapter imports (`external/…`);
- a ready step (`st3-work:…`), fault (`st3-fault:…`), or gh watch event (`github-watch`);
- a work handoff (`st work handoff`);
- daemon run reports, provider-capacity retry nudges, product-wait and planning notices.

Daemon-built notifications bypass ordinary conversation acceptance. Their explicit event families always wake. A plain conversation described as a handoff needs `--question` to wake a questions-only seat. `--question` is a cooperative cost filter, not access control; st never infers questions from the body. Ordinary senders cannot assert the reserved daemon event tags.

## How it works

The daemon decides once, when it accepts a message. It stores a held message with the `st3-fyi`
tag; a message the setting held also carries `st3-fyi-held-by-setting`. It reads the recipient's
declaration only when the answer depends on it. Every later reader classifies a message from its
own tags and sender, so the delivery path makes no extra query for a message. Messages already
sent keep the delivery they were sent with when the setting changes.

A seat's mailbox stream keeps held mail admitted, but it leaves unoffered held mail out of the
frame it sends until a message that wakes the seat is in it. At most the eight newest unoffered FYIs go out with that wake, in send order.
A line reports `K older FYI held; st conversations ls`; the remaining mail stays held for
on-demand reads or a later wake. Each harness shows a held message with
`(FYI: held without waking you until this turn)`. Once offered, held mail follows the ordinary
staged, delivered and read receipts. Reconnects load bodies for at most eight held subjects, regardless of age; all others remain durable.

Held mail is never lost. Until it is read it is unread like any other message:
`st conversations ls` lists it, and `st conversations read` reads it on demand. Held mail waiting
for a turn does not count as late in the `message-delivery` check, does not keep a stopping seat
running, and does not block a rollout drain. `st doctor`'s `held-mail` check lists each seat with
up to 128 oldest unoffered FYIs unread for over a day, selected from an indexed held flag. Backlog cleanup excludes unoffered FYIs.

## Coordination cost

`st usage --hours 24 --messages-only --json` reads counts without message bodies or token
usage: `agent_to_agent`, its held `fyi` subset, and `to_person`. Each message subject counts
once, including messages already read. Daemon and person senders, delivery probes, soaks and explicit synthetic test traffic do not count. The covering
metadata index is maintained by writers; historical bootstrap advances at most eight subjects per
writer batch, stopping between subjects once it has spent ten milliseconds. One daemon task
awaits each bootstrap job on the existing FIFO writer, pauses 100 milliseconds between jobs, and stops
when complete; per-claim projections cannot multiply the historical budget. `complete: false` means bootstrap is unfinished, so collectors publish no ratio.
Reads never advance bootstrap. The factory SLO timer divides each count by merges in the
rolling day and publishes informational values; zero merges gives no ratio.

## Upgrades and recovery

Old daemons silently ignore the typed `fyi`/`question` parameters and do not hold FYI tags;
those messages can wake their recipients. Old CLIs reject the new flags. The CLI warns when
an agent's requested FYI is absent from the receipt. Swift and TypeScript receipt warnings
are deferred; their callers must verify returned tags when using mixed versions.
Upgrade every owner daemon before setting `wake-on`: an old owner treats that declaration
revision as a changed launch and can restart the seat. A downgrade has the same risk and
removes no-wake delivery guarantees. Rust callers constructing `MessageSendParameters`
explicitly must supply the two new boolean fields.

Bootstrap reports `cursor`, `ceiling`, `progress_ms` and `complete` from local metadata;
doctor warns if unfinished bootstrap makes no progress for 15 minutes. On reopening after
an old binary ran, the writer reopens the uncovered send tail in bounded jobs. Corrupt source
claims are not skipped: bootstrap fails closed and keeps counts incomplete so a collector
cannot publish a plausible undercount. Repair the source using existing recovery procedures.
