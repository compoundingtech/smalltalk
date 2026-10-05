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

Windows complete independently: a slow collection read does not prevent new
subscription commands or ready conversation and terminal frames from being
handled. Each subscription still delivers its snapshot before its changes.
Replacing or removing a subscription discards its pending collection result;
changes observed during a read schedule another read of that window.
Each socket permits at most eight physical collection reads, including store
work still finishing after cancellation. Replacements wait for a read slot
without blocking command admission or unrelated ready frames.

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

## Conversations

A conversation is one more subscription too. Name an agent or a session:

```json
{"kind":"subscribe","id":"talk","collection":"conversation","conversation":"agent/ID"}
```

An agent's conversation is its current session's timeline with the Smalltalk to or from the agent
joined in. The first `conversation` frame carries `id`, `collection` (`conversation`),
`session_id`, `replace: true`, the newest page of timeline `items`, `has_more`, and
`preparation: "ready" | "miss"`. The page and its replay cursor come from one
authoritative initial read, not a second timeline request. Later frames
carry `replace: false` and the entries that changed since; an entry revised in place arrives
again with its new revision. When too much changed for one frame, or a change can no longer be
replayed, the newest page arrives again with `replace: true`. An `error` frame ends that
subscription only, for example while the owning host is unreachable. After a dropped socket,
subscribe again on the new one.

Held agents windows prepare their returned current sessions without hover,
selection, or a client prefetch call. Background reads share one global 10%
sustained read-duty budget and end with that window or socket. Unrelated fleet
commits do not rebuild every conversation: a moving global message window
invalidates only agents losing retained messages. Every hit checks authority,
changes concerning its agent, native binding, and file identity.
Prepared pages retain their pagination backing under the 64 MiB cache bound.
Native snapshot races trigger resync and retry without ending the subscription.
`ready` includes an unchanged native-unavailable notice; it does not mean
native-complete. A miss reads the full current page. Neither label promises a
latency for an unprepared daemon or replaces actual content.

Conversation routing resolves the runtime identity at the requested snapshot
without building the fleet's agent cards. Message reads apply the conversation's
party and store-index bounds before decoding message bodies, preserving the
same global message window, session attribution, timeline order, and page size.

`st missions ls --watch`, `st attention ls --watch`, `st agents ls --watch`,
and `st work ls --watch` consume this same transport.
