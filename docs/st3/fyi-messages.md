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
| A question or a handoff | `st conversations send` / `reply` | the recipient |
| Anything else | `st conversations send --fyi` | nobody |

## A seat that wakes only on questions

```kdl
agent "example/builder" {
  wake-on "questions"
}
```

`st agents wake-on agent/example/builder questions --as agent/example/builder` sets the same
choice without restarting the seat; `all` (the default) restores it. Under `questions`, every
message that asks the seat nothing is held as FYI. A message asks the seat something when:

- its sender declared it a question (`--question`), or
- it belongs to a thread the seat asked a question in, so it is the answer the seat waits for.

st never guesses from the words. Every message in a thread carries a
`st3-question-thread:<asker>` tag for each seat that asked in it, copied from parent to reply.

These always wake, whatever the seat chose and whatever the sender marked:

- a person's message, including one an adapter imports (`external/…`);
- anything st itself sends (`daemon/…`): a ready step, a fault, a gh watch event, a person's answer;
- a work handoff (`st work handoff`).

## How it works

The daemon decides once, when it accepts a message. It stores a held message with the `st3-fyi`
tag; a message the setting held also carries `st3-fyi-held-by-setting`. It reads the recipient's
declaration only when the answer depends on it. Every later reader classifies a message from its
own tags and sender, so the delivery path makes no extra query for a message. Messages already
sent keep the delivery they were sent with when the setting changes.

A seat's mailbox stream keeps held mail admitted, but it leaves unoffered held mail out of the
frame it sends until a message that wakes the seat is in it. Then they go out together, oldest
first, at the start of that turn. Each harness shows a held message with
`(FYI: held without waking you until this turn)`. Once offered, held mail follows the ordinary
staged, delivered and read receipts. A seat that reconnects still receives every unoffered held message with its next wake,
regardless of its age.

Held mail is never lost. Until it is read it is unread like any other message:
`st conversations ls` lists it, and `st conversations read` reads it on demand. Held mail waiting
for a turn does not count as late in the `message-delivery` check, does not keep a stopping seat
running, and does not block a rollout drain. `st doctor`'s `held-mail` check lists each seat with
FYI mail unread for over a day.

## Coordination cost

`st usage --hours 24 --messages-only --json` reads counts without message bodies or token
usage: `agent_to_agent`, its held `fyi` subset, and `to_person`. Each message subject counts
once, including messages already read. Daemon and person senders do not count. The covering
metadata index is maintained by writers; historical bootstrap advances at most eight subjects per
writer batch, stopping between subjects once it has spent ten milliseconds. One daemon task
awaits each bootstrap job on the existing FIFO writer, pauses 100 milliseconds between jobs, and stops
when complete; per-claim projections cannot multiply the historical budget. `complete: false` means bootstrap is unfinished, so collectors publish no ratio.
Reads never advance bootstrap. The factory SLO timer divides each count by merges in the
rolling day and publishes informational values; zero merges gives no ratio.
