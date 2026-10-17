# Current collection subscriptions

Connect once to `GET /v1/client/collections/stream` with WebSocket subprotocol
`st3.client.collections.v0`. The same Unix or paired client credentials used by
the rest of client v0 apply. One connection accepts up to sixteen subscriptions.
The optional `collections` capability version 1 advertises this bound; older
daemons omit it and accept eight. Clients reserve their existing conversation
and terminal slots before adding optional windows on older daemons.

The server sends a WebSocket protocol ping every eight seconds and answers client
pings with the same payload. These frames do not read collection windows. A send
that cannot complete within eight seconds closes the socket; reconnect and apply
the new authoritative snapshot.

Send one JSON command per subscription:

```json
{"kind":"subscribe","id":"missions-tab","collection":"missions","limit":50}
```

Collections are `missions`, `attention` (also subscribable as `alerts`, the name a person reads; frames name it `attention`), `agents`, `work`, `glasses`, and `arrangements`,
plus `terminal` and `conversation` (below). The optional `actor` filter applies to work,
`person` to attention, and `status` to agents. For `arrangements`, `person: "person/NAME"`
is required: agents explicitly select a fleet person's collection, never an inferred
owner. Each read checks `read.arrangements` and the selected person's access. For example:

```json
{"kind":"subscribe","id":"sidebar","collection":"arrangements","person":"person/ada","limit":100}
```

To follow one selected arrangement, add optional `subject: ArrangementId`:

```json
{"kind":"subscribe","id":"sidebar","collection":"arrangements","person":"person/ada","subject":"arrangement/person/ada/019a0000-0000-7000-8000-000000000002"}
```

The subject must belong to `person`; a mismatched owner is refused. This window
contains only that subject (zero or one items), independent of the owner's
subject-ordered count/byte prefix. It receives a snapshot, full-resource upserts
and the normal retirement removal, with `has_more: false`. An absent or retired
subject starts empty. Individual resource byte bounds still apply. Omitting
`subject` retains the existing owner-wide bounded window.

Arrangement snapshots and changes carry full typed arrangement resources. Folder or
placement changes are full resource upserts; retirement sends the arrangement ID in
`removes`. Glass privacy and its person-only selection are unchanged.
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
snapshot's store index, so rows always match their fence. Agents, missions and
work windows serve lists a background refresher keeps published: their fence
names the published list's own cut and `published_at` (see the client README's
freshness sections). Commits that land
while a window is read neither tear it nor delay it; they arrive in the next
`changes` frame.

Windows complete independently: a slow collection read does not prevent new
subscription commands or ready conversation and terminal frames from being
handled. Each subscription still delivers its snapshot before its changes.
Replacing or removing a subscription discards its pending collection result;
changes observed during a read schedule another read of that window.
Each socket permits at most sixteen physical collection reads, including store
work still finishing after cancellation. Replacements wait for a read slot
without blocking command admission or unrelated ready frames.

An `error` frame reports a permanent refusal and ends that subscription. A
`resync` frame with `retryable: true` reports a temporary read failure; the server
keeps the subscription and retries after its reread interval, including when the
first snapshot failed. Once admitted, an opening remote conversation subscription
may start retries for `remote-unavailable` during the three seconds after its first
failed opening read, with 250 ms between attempts. This bounds when retries start,
not when outage diagnostics arrive. An in-flight retry keeps the normal peer RPC
deadline: a stalled retry can wait about 15 seconds before reporting `resync`, even
after the retry-start window has closed. A slow successful page is not canceled by
that window. A persistent outage still reports `resync`; other refusals and
established conversations do not use this grace.
A followed conversation's `resync` also carries the failure's `code` and `message`,
so a client still showing its last copy can say that copy is stale. Clients may resubscribe with the same ID to request a
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

An agent's conversation is its current session's timeline with the Fractal to or from the agent
joined in. The first `conversation` frame carries `id`, `collection` (`conversation`),
`session_id`, `replace: true`, the newest page of timeline `items`, and `has_more`. Later frames
carry `replace: false` and the entries that changed since; an entry revised in place arrives
again with its new revision. When too much changed for one frame, or a change can no longer be
replayed, the newest page arrives again with `replace: true`. An `error` frame ends that
subscription only, for example when a session is absent or access is forbidden.
When an owner read fails, retryable `resync` retains the admitted subscription.
After a dropped socket, subscribe again on the new one.

`has_more` describes history before the replacement window, not whether a delta has more
changes. Delta frames omit it: absence means preserve the last known availability, never
`false`. Clients retain this distinction through decoding and UI updates; an explicitly
supplied boolean updates availability. A replacement (including a reconnect) or a changed
session starts with its own availability and must not inherit the previous window's value.
Older-page responses independently report whether there are entries before that page; once
the session's start is reached, later live deltas do not reopen older-history paging.

`st missions ls --watch`, `st attention ls --watch`, `st agents ls --watch`,
and `st work ls --watch` consume this same transport.

## Summary

A daemon that advertises the `summary` capability version 1 accepts
`{"kind":"subscribe","id":"top-bar","collection":"summary","limit":1}`.
A person session uses its own authority; an agent may select `person` using the
same access rules as attention. This is one row, `summary/current`, with
`kind: "summary"`, `person_id`, `alerts`, `needs_you`, `working_agents`, `active_missions`,
and `machines: {connected, indirect, offline}`. Normal snapshot/changes/order
frames apply, with `has_more: false`. There is no new HTTP list or get route.

These are complete current source counts, independent of any list's page size.
`alerts` counts open alerts: items that block or wait on the person (human
gates, launch/revision approvals, person asks, agent and custom requests, harness
prompts and logins, and broken gates the person published). Updates, messages and
agents' faults are not alerts. `needs_you` carries the same count for older clients; a daemon that
predates alerts omits `alerts` and also counted unread updates in `needs_you`. Local snoozes and closed rows remain
client-owned. `working_agents` uses running/working presentation, excluding
faults and stale delivery. `active_missions` uses the shared mission words
working, queued, decision, stalled, unstaffed and unclaimed, excluding system
missions. Human gates use the selected person's unresolved attention membership.
Machines use direct/gateway connectivity, otherwise the newest transport or
agent activity strictly less than five minutes old, otherwise offline.

Native counts use the existing authorized snapshot and collection clock. Lean
mission inputs reuse the shared mission word calculation, omit card history and
provenance, and are shared across activity updates. Cached inputs expire at lease, future-request
and recent-outcome boundaries and reject backward clock reuse. Agent cards reuse the native
keyed roster cache; live delivery overlays are checked on every read. Authority
is revalidated before counting, including private person selection. Read failures
send the existing `resync`/`error` frames, never inferred zero counts. Clients keep compatibility
behavior on older daemons.
A daemon serves these counts from published rows. What no person changes (missions,
working agents, machines) is computed once for every selection, at one cut, and again
when the cut, the roster or the 30-second clock period changes. The machines they count
are read again only when a claim about a host, or a run or generation that owns runtimes,
arrives, when a runtime with new claims (other than its harness reports) starts on, stops
on or moves to another host so that the set of runtime hosts changes, when operations
conflict or fleet membership changes, and each clock period, when peers' replication
successes are read again. Each selection counts
its alerts from the published attention rows, the same rows its
attention window shows. The row carries the shared counts' cut. A window that opens a
new selection is served at once. Before the first publication, the window gets a
retryable `summary-not-ready` resync. The shared counts read the complete agents roster,
so as a daemon starts they are published as soon as that roster is.
Equal count membership produces no upsert, including agent progress/usage
updates and another person's attention changes. Core list/filter/get behavior
is unchanged. Summary freshness is carried by its snapshot fence; equal counts
retain their prior row timestamp so an unchanged count emits no delta.

## Presentations

Optional capability `presentation` version 1 says agent rows carry `presentation`: st's state
word, list group, why the agent needs someone, and what it holds and has queued (see
[the UI contract](../../clients/ui-contract.md#agent)). Older daemons omit both. A row that
lacks it shows the state st reports, marked as coming from an older st; clients derive nothing.

## Sync on the stream

A window frame of the `attention` collection may carry `sync` (`StreamSync`): this host's
replication, taken when the frame was built. It is `{state: "level"}` while this host is current,
or `{state: "catching-up" | "diverged", peers}` naming the peers it lacks envelopes from or
projects a different graph than, with the same `SyncPeer` a page's `sync` carries. Unlike a page's
`SyncNotice`, which is absent while level, it says `level` explicitly: on the stream an absent
`sync` means unknown, as from a daemon that does not send it, never level.

A daemon that sends it puts it on every `snapshot` frame of the collection, and on a `changes`
frame only when it changed since the subscription's previous frame: its state, or which peers
(by `host_id`) are named, including the change back to `level`. A peer's counts moving is not a
change. A client that missed a changes frame learns the current value from the next snapshot,
which every resubscribe gets. A client on a daemon that sends none keeps reading a page's `sync`
(`GET /v1/client/attention?limit=1`).

A state this client does not know, or a shape the schema forbids (`level` with peers, or another
state without them), reads as unknown, never level, and the frame is kept: the Rust client reads
such a `sync` as none, and the TypeScript transport parses frames without validating them, so a
reader there must apply the same rule (only `level` without peers is level; only `catching-up` or
`diverged` with peers is behind; anything else is unknown). Decoding a frame with the generated
TypeScript schema is stricter: it rejects a forbidden shape as it rejects any malformed field,
and a state it does not know decodes as unknown. A daemon sends neither. Older clients ignore the
field: the generated Rust and Swift decoders read frames by named field (fixtures:
`fixtures/clients/stream-sync/frames.json`).

## Screen inventories

Optional capability `inventory-collections` version 1 advertises six additional
collections: `clients`, `usage`, `machines`, `devices`, `sessions`, and `terminals`.
Older daemons omit it; clients keep their existing HTTP reads for that member and
say that the member is read, not followed. These subscriptions use the existing
v0 `snapshot` then `changes` frames, including removals, complete order and fences.
Each subscription consumes one of the
sixteen slots, including the terminals **list**, which is separate from a selected
`terminal` screen.

All six require `read.projections`, as their HTTP reads do. Devices select the
session's authenticated person, or an explicitly authorized `person`, using the
same rules as `GET /v1/client/devices`. Only active devices are included. Sessions
contain native sessions (`native_only=true`), with managed sessions excluded.
Machines and terminals contain the same current rows as their HTTP lists. These
inventories accept a `limit` of 1–200 (default 50). History remains on HTTP. Devices
accept `person`; the other five accept no person, actor, subject or status filter.

Clients use `kind: "connected-client"` resources with the HTTP connection fields
and a stable ID derived from member, actor, reported client name and route.
Connect, disconnect, followed-window changes and requests wake the collection;
recent request-only rows leave at their five-minute expiry. Opening a stream does
not require a graph write. Reconnect always returns a fresh snapshot of presence.

Usage has one `usage/current` resource with `kind: "usage"` and `value: UsagePeriod`:
rows, account limits and agent-message estimates come from the same read as
`usage.period`, inside the frame's SQLite snapshot. `limit` bounds its nested
usage rows, ranked as on HTTP; `has_more` means more usage rows exist. Limits and
message estimates describe the whole period. Send millisecond boundaries:

```json
{"kind":"subscribe","id":"usage-tab","collection":"usage","since_ms":1791763200000,"until_ms":1791849600000,"limit":200}
```

An omitted `until_ms` follows new usage through the current read time; an omitted
`since_ms` starts 24 hours before that end. An explicit end remains fixed across
changes and reconnects. New usage, limit readings and message sends wake this
collection; an idle report has no periodic reread. Paired inventory authority expires at its grant deadline, without periodic list
reads. Native sessions also follow
transcript filesystem events and, on Linux, observed process exits. Device expiry
and recent-client expiry schedule their actual deadlines rather than polling.

Keep `clients` open only in Fleet, `usage` only in Usage, `devices` and `sessions`
only where their lists show, and `terminals` only where the terminal chooser/list
shows. `machines` stays open while the sidebar is visible (including Fleet).
Existing always-held attention, missions, agents, work, glasses, arrangements and
summary windows retain their client policy. Conversation and terminal-screen slots
are reserved before optional list windows. Leaving a screen unsubscribes its
inventory after the client's existing grace period. Pull to refresh replaces the
subscription with the same ID to obtain a fresh authoritative snapshot.
