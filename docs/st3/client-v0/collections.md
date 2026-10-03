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
path, title, first goal, and state, and the `subagents` its harness runs now.
A work row names its `mission_id`.

Each window is read inside one SQLite snapshot, and its fence names that
snapshot's store index, so rows always match their fence. Commits that land
while a window is read neither tear it nor delay it; they arrive in the next
`changes` frame.

An `error` frame reports a permanent refusal and ends that subscription. A
`resync` frame with `retryable: true` reports a temporary read failure; the server
keeps the subscription and retries after its reread interval, including when the
first snapshot failed. A followed conversation's `resync` also carries the failure's
`code` and `message`, such as `remote-unavailable` while the owner's host cannot be
reached, so a client still showing its last copy can say that copy is stale. Clients may resubscribe with the same ID to request a
fresh snapshot. If the socket closes, including during a daemon
restart, open a new socket and subscribe again; the new snapshot is authoritative.
Each socket subscribes to store changes before taking its first snapshot, so a
write racing that snapshot is visible in the snapshot or a subsequent frame.

## Terminals

A terminal is one more subscription on the same socket. Call `terminal.attach`
first, then subscribe with the terminal ID, the incarnation it fenced, and the
`stream_capability` it returned. The capability is a reusable lease (see `ttl_s` and
`expires_at` on the attachment): a client that reconnects subscribes again with it until it
expires or the viewer is detached:

```json
{"kind":"subscribe","id":"term","collection":"terminal","terminal":"terminal/ID","incarnation":"INCARNATION","capability":"CAPABILITY"}
```

Each `screen` frame carries `id`, `collection` (`terminal`), `snapshot`, and
`value`, a whole `TerminalScreen` that replaces every earlier one. The first is
the current screen; later ones arrive only when the screen changes, at most
every 100 ms, and nothing while it is idle. A slow client gets the latest
screen, never a backlog, and one terminal never holds back the socket's other
subscriptions. An incarnation change ends that subscription with `stale-fence`.
A process exit returns `terminal-ended` with `retryable: false`. A temporary
owner, I/O or viewer-idle failure returns `terminal-unavailable` with
`retryable: true`; clients subscribe again with the same lease, or attach again once it has
expired. Unsubscribing, or closing the socket, stops following; `terminal.detach` still ends the
viewer record and revokes the lease. After a dropped socket, subscribe on the new socket.

### Terminal input

A device with `terminal.control` writes to a terminal it follows on the same
socket by opening input on that follow:

```json
{"kind":"input-open","id":"keys","follow":"term"}
{"kind":"input","id":"keys","seq":0,"data":{"text":"ls\r"}}
{"kind":"input","id":"keys","seq":1,"data":{"bytes_b64":"Gw=="}}
{"kind":"input-close","id":"keys"}
```

The input inherits the follow's consumed viewer and fenced incarnation, so it
needs no second capability. `input-opened` carries `id`, `follow`, and
`next_seq`, the first sequence to send. Each batch holds exactly one of `text`
(written as its UTF-8 bytes) or `bytes_b64`, 1 to 16384 bytes with no NUL byte,
and is acknowledged with `input-ack` (`id`, `seq`) once written. A `seq` below
`next_seq` is a repeat: it is acknowledged again and never written. Sequences
apply in order: a `seq` above `next_seq` closes the input with `gap`.

Before every batch the server checks that the device is still paired, the
viewer was not detached, and the terminal still runs the follow's incarnation.
`input-closed` (`id`, `reason`, `message`) ends the input, and nothing more is
written: `incarnation-changed` when the incarnation changed or the process
ended, `revoked` when the pairing was revoked or expired, `detached` when the
viewer was detached or its follow ended or was unsubscribed or replaced, `gap`,
and `rejected` for a refused open or batch, including a device without
`terminal.control`, a follow another host owns, or a follow that already has
input. Batches for a closed input are dropped without a frame. The server never
resends; after a close, attach again and open a new input with fresh bytes.
`input-close` ends an input without a frame. An input ID held again replaces
the earlier input.

## Conversations

A conversation is one more subscription too. Name an agent or a session:

```json
{"kind":"subscribe","id":"talk","collection":"conversation","conversation":"agent/ID"}
```

An agent's conversation is its current session's timeline with the Small Talk to or from the agent
joined in. The first `conversation` frame carries `id`, `collection` (`conversation`),
`session_id`, `replace: true`, the newest page of timeline `items`, and `has_more`. Later frames
carry `replace: false` and the entries that changed since; an entry revised in place arrives
again with its new revision. When too much changed for one frame, or a change can no longer be
replayed, the newest page arrives again with `replace: true`. An `error` frame ends that
subscription only, for example while the owning host is unreachable. After a dropped socket,
subscribe again on the new one.

`st missions ls --watch`, `st attention ls --watch`, `st agents ls --watch`,
and `st work ls --watch` consume this same transport.
