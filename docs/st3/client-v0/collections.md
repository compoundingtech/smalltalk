# Current collection subscriptions

Connect once to `GET /v1/client/collections/stream` with WebSocket subprotocol
`st3.client.collections.v0`. The same Unix or paired client credentials used by
the rest of client v0 apply. One connection accepts up to eight subscriptions.

Send one JSON command per subscription:

```json
{"kind":"subscribe","id":"missions-tab","collection":"missions","limit":50}
```

Collections are `missions`, `attention`, `agents`, and `work`, plus `terminal` (below). The optional
`actor` filter applies to work, `person` to attention, and `status` to agents.
A window contains 1–200 current items. History remains on the
corresponding paged HTTP reads. Send `{"kind":"unsubscribe","id":"missions-tab"}`
to remove a subscription. IDs are chosen by the client and unique on the socket.

The first reply for each subscription is a `snapshot` frame with `id`,
`collection`, `snapshot`, `items`, `order`, and `has_more`. `items` are the same
joined resource values as client v0 list reads. `order` contains their IDs in
display order. Later `changes` frames carry `upserts`, `removes`, the complete
new `order`, `has_more`, and a new snapshot fence. Apply removals and upserts
before displaying the new order. Rows leaving a bounded window appear in
`removes`, even if they still exist beyond that window.

Rows are joined in st so a client never joins collections itself. A mission's
`run_details` carry `steps` for each open run and for its latest run, and every
run's steps in a mission detail read. An agent names its queue in
`current_work`, `next_work`, and `upcoming_work`: each step's mission, run,
path, title, first goal, and state. A work row names its `mission_id`.

A snapshot never pairs rows with an older fence. When a commit lands while a
window is read, the server reads it again, and after repeated races it waits
briefly and tries once more; the client never sees the race.

An `error` frame reports an invalid subscription. A `resync` frame tells the
client to subscribe again. If the socket closes, including during a daemon
restart, open a new socket and subscribe again; the new snapshot is authoritative.
Each socket subscribes to store changes before taking its first snapshot, so a
write racing that snapshot is visible in the snapshot or a subsequent frame.

## Terminals

A terminal is one more subscription on the same socket. Call `terminal.attach`
first, then subscribe with the terminal ID, the incarnation it fenced, and the
single-use `stream_capability` it returned:

```json
{"kind":"subscribe","id":"term","collection":"terminal","terminal":"terminal/ID","incarnation":"INCARNATION","capability":"CAPABILITY"}
```

Each `screen` frame carries `id`, `collection` (`terminal`), `snapshot`, and
`value`, a whole `TerminalScreen` that replaces every earlier one. The first is
the current screen; later ones arrive only when the screen changes, at most
every 100 ms, and nothing while it is idle. A slow client gets the latest
screen, never a backlog, and one terminal never holds back the socket's other
subscriptions. When the incarnation changes or the process exits, an `error`
frame with code `stale-fence` ends that subscription only. Unsubscribing, or
closing the socket, stops following; `terminal.detach` still ends the viewer
record. After a dropped socket, attach again and subscribe on the new socket.

`st missions ls --watch`, `st attention ls --watch`, `st agents ls --watch`,
and `st work ls --watch` consume this same transport.
