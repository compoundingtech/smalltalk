# st client v0 contract

Status: implemented client boundary with operational projections, resumable events, authenticated
pairing, fenced actions, streamed terminal screens, and generated Rust and Swift clients. The files
in [`schemas`](schemas) and [`fixtures`](fixtures) are the normative wire examples. Rust and Swift
clients consume the same JSON; no client parses CLI output, Markdown, KDL, claim envelopes, or
harness transcript files.

The reusable Rust package is [`crates/st3-client`](../../../crates/st3-client) and supports both the
local Unix socket and authenticated paired HTTP over Tailscale or optional Fabric. The Swift package is
[`clients/swift/St3Client`](../../../clients/swift/St3Client). The Expo TypeScript client is
[`clients/typescript/st3-client`](../../../clients/typescript/st3-client). Regenerate all three
clients' contract tables with `cargo run -p st3-client-codegen`;
CI and local verification use `cargo run -p st3-client-codegen -- --check` for byte stability.
The contract digest is written once, in `crates/st3-client/src/contract.rs`. When generated
client files conflict in a merge, resolve the schema and template files, take either side of every
generated file (`git checkout --theirs -- clients crates/st3-client/src/contract.rs
crates/st3-client/src/generated.rs`), and run the generator again; never edit generated files by
hand.

`ResourceHeader.operational` describes current versus historical state, actionability, reasons,
and optional owner-generation/runtime-incarnation identities. Work also exposes `agentless`,
claim expiry, execution start/elapsed time, and timeout in milliseconds. These fields are optional
for older servers; an absent expiry/start/timeout and an explicit null both mean no value.
Mission visualization and message `reply_to` may likewise be null. Agent reachability includes
`reachable` and `indeterminate`; machines use the `machine` resource kind.
Mission cards also declare total runs, per-state run counts, and whether their recent-run window
is truncated. Sessions expose their optional runtime incarnation. Clients need not infer these
fields from untyped excess-property bags.

The collections socket's `CollectionCommand` and `CollectionFrame` definitions live in the same
schema as HTTP resources. The operation manifest's `streams` section names its route, protocol,
command/frame definitions, and subscription bound; see [collections](collections.md).

### Agent activity and human blocking

An agent's `harness_state` describes activity independently of its optional `blocked_on`, `ask`,
and diagnostic `reason`. A current `blocked_on: "human"` observation with ready, working, or idle
activity makes the canonical agent `state: "waiting"`; clients present that combination as needing
a person. It takes precedence over working or idle, not over a terminal runtime, an ended/failed or
indeterminate harness, a reconcile fault, or an observation fenced out by the current incarnation.
`ask` names the structured question, permission, or review, not text inferred from the terminal.

The omp extension correlates an ask with its tool-call ID. Unrelated results leave it blocked; the
matching answer emits a new unblocked activity frame. The pi-family channel retains all three axes
while retrying publication and across st binary replacement. Each published harness observation is
a complete snapshot, including explicit null clearing for absent blocking metadata, composer state,
and exit, so legacy optional-field backfill cannot resurrect an answered ask. A delayed observation
from a previous runtime incarnation never changes the current agent.

### Subagents

An agent's `subagents` lists the subagents its harness runs now, oldest first: each was recorded
on the seat, has not ended, and has a lease that runs past the read. Each one has the harness's
own `id`, its `subagent_type`, a one-line `description`, its `driver` and `session_id`, the
`work_id` of the step the seat held when it appeared, `started_at`, and `lease_expires_at`. A
subagent is part of its agent, not a resource of its own, and has no actions. The list is read
per request, so a lease that runs out leaves it at the next read without a new claim. A daemon
older than the field omits it; clients read a missing list as empty.

### Declared resources

An agent's or mission's `resources` lists the resources its current declaration names, in authored
order: each has its `name`, the `subject` of the ordinary `uri.reference` resource it points to,
the exact `uri`, and a nullable `reason`. A mission's list comes from its current published
revision. The owner stores typed edges, while the shared smallclaims subject stores the immutable
URI. Its ID is `resource/uri/` followed by the lowercase hex SHA-256 of the exact URI's bytes;
different owners naming that URI resolve to the same subject.

The list is read-only; change it by reapplying the seat declaration or publishing a new mission
revision (see [declared resources](../kdl-lifecycle.md#declared-resources)). Removing an edge does
not delete its subject or other owners' references. `uri` is null while the referenced subject
has not reached the host; the edge still retains its `name`, `subject`, and `reason`. A daemon
older than the field omits it; clients read a missing list as empty.

These declared references are distinct from `GET /v1/client/resources`, which lists resource
observations. Declaring a URI does not fetch it, create an observation, or start an observer.

## Boundary and transport

The client API is a projection and command gateway, not a graph replica. Its version is
`st3.client.v0` and its routes live below `/v1/client`. A local client connects to the daemon's Unix
socket. A remote client connects to the paired-only gateway through a tailnet carrier or optional
Fabric. Never expose the privileged local Unix API through a TCP forwarder.

The gateway authenticates a paired device and derives the actor and scopes for the connection.
Request bodies never select an actor. Device credentials are scoped, individually revocable, and
different from fleet replication secrets. A remote client is always online: v0 has no offline
mutation queue, push notification service, cached graph authority, or multi-master replication.

### Tailnet carrier

`st up` listens on two different Unix sockets. `st3.sock` is the privileged trusted-local API;
`st3-client.sock` is the paired-only client gateway backed by `fabric_router`. The latter rejects
ordinary requests without a paired bearer credential, except for pairing completion. The socket
paths can be set with `socket` and `client_gateway_socket` in `config.toml`, or with `--socket` and
`--client-gateway-socket` for a foreground daemon. They must never name the same path.

For a direct tailnet connection, forward a TCP listener bound to the host's Tailscale IP to
`st3-client.sock`. Pair the phone with `http://TAILSCALE_IP:PORT`. Tailscale encrypts the link;
the paired gateway still authenticates each client request. Never bind this listener to a public
or LAN interface, and never forward `st3.sock`. An unauthenticated request to
`/v1/client/capabilities` must return a complete `403`; follow it with an authenticated read and
concurrent-read check. Run the forwarder as a persistent service so daemon restarts do not strand
the client. On iOS 17 and later, an App Transport Security exception can target Tailscale's
`100.64.0.0/10` range without opening arbitrary HTTP destinations.

On a shared LAN, the same paired-only socket can be forwarded from a listener bound to one private
LAN IP and reached through that address or the host's `.local` name. iOS needs Local Network
permission and an App Transport Security local-network allowance. Plain HTTP on the LAN exposes
the paired bearer credential to anyone able to observe that LAN traffic; pairing authenticates
requests but does not encrypt this transport. Keep the listener on an explicit private interface,
and use HTTPS or the tailnet route when the LAN is not trusted.

Tailscale Serve remains an optional HTTPS carrier for clients that need it. If using Serve,
publish only the paired-only socket:

```sh
CLIENT_GATEWAY_SOCKET="${XDG_RUNTIME_DIR:-$HOME/.local/state/st3/run}/st3-client.sock"
tailscale serve --bg --yes "unix:${CLIENT_GATEWAY_SOCKET}"
tailscale serve status
```

On macOS, verify the HTTPS route with an unauthenticated request to
`/v1/client/capabilities`: the gateway should return a complete `403` response.
If Tailscale Serve returns `502` when pointed at the Unix socket, run a persistent
launchd-managed bridge from a loopback-only TCP port to `st3-client.sock`, then
point Tailscale Serve at that port. The bridge must bind `127.0.0.1`, reconnect to
the socket for each request, and start independently of the st daemon so a daemon
restart does not leave the HTTPS route pointing at an absent process. Recheck the
HTTPS route after every rollout, then make an authenticated paired-client read.

This provides tailnet-only HTTPS and WebSocket transport at the host's Tailscale name while the
gateway continues to enforce the same paired credential, scopes, terminal subprotocol, and
attachment capability. Begin pairing over the trusted local socket with, for example,
`st devices --as person/alex pair "Alex iPhone"`; complete pairing from the remote device over
the served gateway. To remove the carrier without changing graph credentials or daemon state:

```sh
tailscale serve reset
```

Warning: `tailscale serve reset` clears all Serve configuration on the host, not only the st
gateway.

The equivalent trusted-peer Fabric carrier lifecycle is:

```sh
fabric expose st3-client-v0 --socket /ABS/st3-client.sock
fabric dial HOST st3-client-v0
fabric unexpose st3-client-v0
```

Expose and unexpose change only the socket exposure; existing peer grants stay intact and remain
the authority for access. Daemon startup never runs either carrier command, enables Funnel, or
changes Tailscale/Fabric policy.

Never point `tailscale serve` at `st3.sock`. That socket intentionally carries privileged local
control routes and is not an authenticated remote-client boundary. The transport conformance test
starts both Unix routers, proves the gateway rejects an ordinary local client, completes pairing on
the Fabric-like carrier, and exercises authenticated HTTP plus terminal WebSocket traffic through
the paired-only socket.

Every JSON response has `api_version`, `request_id`, and either `value` or a versioned error. Read
responses also carry one `snapshot`:

```json
{
  "api_version": "st3.client.v0",
  "request_id": "req/0199...",
  "snapshot": {
    "id": "snapshot/host-a/1842/2fc9...",
    "host_id": "host/host-a",
    "store_index": 1842,
    "projection_version": "client-projection.v0",
    "created_at": "2026-09-20T12:00:00.000Z"
  },
  "value": {}
}
```

A snapshot ID identifies the complete projected state at one local store index and projection
version. All items in a response are computed in one read transaction. A page cursor is opaque,
bound to the collection, filter, page size, and final sort tuple, with an advertised
`cursor_expires_at`. Unrelated commits do not invalidate a page. Cached collections continue from
the original snapshot; missions and work history seek after the last timestamp and ID in a fresh
read transaction and explicitly name that page's snapshot. A fresh page sequence observes rows
that move ahead of the continuation position; current collection subscriptions supply live
updates. Real retention expiry still returns
`page-cursor-expired`, so clients restart the page sequence.

## Discovery, lists, and details

`GET /v1/client/capabilities` is the first request. It returns the negotiated limits, schema URIs,
feed retention boundary, authenticated session actor, and capabilities. Unknown required capability
versions stop the client; unknown optional capabilities may be ignored.

The following resources have list and detail routes. List routes accept `limit` and `cursor` plus
documented filters. The server clamps `limit` to `capabilities.limits.max_page_items`. Empty values
sort after present values, strings compare as Unicode scalar sequences, and the final key is always
the stable `id` ascending. No locale-sensitive ordering is permitted.

| Resource | List and detail routes | Deterministic list order |
|---|---|---|
| Attention | `/attention`, `/attention/{id}` | priority descending, requested time ascending, ID |
| Messages | `/messages`, `/messages/{id}` | sent time descending, ID |
| Launches | `/launches`, `/launches/{id}` | updated time descending, ID |
| Launch variants | `/launches/{id}/variants`, `/launches/{id}/variants/{variant_id}` | ordinal, ID |
| Launch decisions | `/launches/{id}/decisions`, `/launches/{id}/decisions/{decision_id}` | requested time, ID |
| Launch approvals | `/launches/{id}/approvals`, `/launches/{id}/approvals/{approval_id}` | decided time, ID |
| Missions | `/missions`, `/missions/{id}` | updated time descending, ID |
| Work | `/work`, `/work/{id}` | ready first, readiness epoch, path, ID |
| Agents | `/agents`, `/agents/{id}` | presentation name, ID |
| Resource observations | `/resources` (list only) | resource subject ID |
| Runtimes | `/runtimes`, `/runtimes/{id}` | owning agent, runtime kind, ID |
| Lanes | `/lanes`, `/lanes/{id}` | open lanes first, ID |
| Operations | `/operations`, `/operations/{id}` | severity descending, component, ID |
| History | `/history`, `/history/{id}` | occurred time descending, store index descending, ID |
| Sessions | `/sessions`, `/sessions/{id}` | updated time descending, ID |
| Session timeline | `/sessions/{id}/timeline` | sequence ascending |

Work resources, mission steps (including `current_steps`), and agent work labels use the same
`WorkState` vocabulary: `waiting-person`, `waiting`, `ready`, `claimed`, `blocked`, `verifying`, `completed`,
`failed`, and `cancelled`. The API translates internal `pending` to `waiting` and `working` to
`claimed` in every projection. Clients treat held or verifying work as active even when its
successors are waiting.

A page carries an optional `sync` notice while its host is catching up with a fleet peer. Its
projections can then show early history as current, such as a person step that a
not-yet-received envelope completes. The notice lists each peer that holds more envelopes than one
replication exchange carries, with `peer_only_envelopes` (held by the peer, missing here),
`local_only_envelopes`, `last_exchange_at`, and `estimated_catch_up_seconds` (null until a rate is
measured). Clients show the notice above the page. The page omits it once the host has caught up.

The notice's `state` is `diverged` instead while the host's graph has diverged from a peer's: both
hold the same envelopes but project different graphs from them, so the page can be wrong, not just
early, and more exchanges will not fix it. Each diverged peer carries `diverged_since`; a notice can
list catching-up peers beside it. Clients say so prominently (stui's header shows `⚠ diverged`)
until the page omits the notice.

IDs are stable opaque strings with a type prefix. Renames change labels, not IDs. A detail response
uses the same representation as its list item plus its documented detail fields. Deletion is
represented by an event tombstone; an ID is never reused.

### Resource observations

`GET /v1/client/resources` lists the latest `resource.observed` claim for each resource subject,
in canonical replicated claim order, not arrival order. It requires `read.projections`, including
for paired devices. The rebuildable indexed projection serves bounded SQL pages rather than
scanning the claim log.

Filters are conjunctive and optional:

- `opened_by=agent/...` matches `facts.opened_by`; `opened_by=mission-run/...` matches
  `facts.opened_by_run`. Other subject types are rejected.
- `kind=vcs.pull-request` matches the resource vocabulary kind, not the claim kind.
- `subject_prefix=resource/github/...` matches a literal subject prefix.
- `limit` and opaque `cursor` use the negotiated page limits. Repeat the same filters when
  continuing a page.

Each item contains `id` (resource subject), `kind` (resource vocabulary), `facts` (the latest
observation's complete facts), `observed_at` (RFC 3339 UTC time at which the latest observation's
claim was accepted), and nullable `opened_by` / `opened_by_run` copied from those facts. Older facts
are not merged into a later observation, so clearing or changing attribution removes the old
filter match.

The response uses the normal `st3.client.v0` envelope and snapshot, with
`value: {kind: "page", collection: "resources", filters, items, page, sync?}`.
`page` contains `limit`, `has_more`, `next_cursor`, and `cursor_expires_at`. Pages retain the first
snapshot while the resource projection is unchanged; unrelated claims do not invalidate them.
A resource projection change or expiry returns `page-cursor-expired` (HTTP 410), and the client
restarts pagination. The cursor is bound to its filters and page size.

Rust exposes `Client::resources_list` with `ResourcesFilter` and a typed resource-observation page;
Swift and TypeScript expose `resourcesList` with equivalent typed filters and items. Resource
observations are distinct from operational resources: their `kind` is an open resource-vocabulary
string, not the operational resource union's discriminator. This list route does not add a
`resources` subscription to `st3.client.collections.v0`.


### Seat workspace directories

`GET /v1/client/agent-workspaces/{id}` reads the declared workspace for one exact seat. `{id}`
accepts `agent/garden/interactive` or `garden/interactive`; encode the path parameter when using
a raw HTTP client. It requires only `read.projections`, not declaration or terminal-control scope.
Rust exposes `Client::agent_workspace_get(id)`; Swift and TypeScript expose `agentWorkspaceGet`.
The CLI is `st agents workspace agent/garden/interactive [--json]`.

The response is `Envelope<AgentWorkspace>`:

```json
{
  "kind": "agent-workspace",
  "agent_id": "agent/garden/interactive",
  "host_id": "host/garden",
  "workspace": "/work/garden",
  "desired_token": "selected-desired-claim",
  "declaration_token": "original-agent-claim"
}
```

The example is the envelope's `value`; its `snapshot.store_index` fences all fields to one SQLite
read snapshot. `workspace` is the declared workspace root on `host_id`, not the connected
gateway's filesystem, a harness state directory, or its process's current subdirectory. The read
does not probe or create the directory and includes no environment values.

Running, mission-owned, suspended, stopped, and retired one-shot seats use the same read; no
history flag is necessary. The server reads the selected declaration, following unambiguous stop
predecessors when needed. `desired_token` identifies the current desired claim (possibly a stop),
while `declaration_token` identifies the agent declaration supplying the path. A replacement
declaration immediately changes the returned workspace. Unknown seats and observed seats without
a managed declaration return `not-found`; conflicting declarations or a stop without an
unambiguous predecessor return `validation-failed`. No directory is inferred from the seat ID.

For the full attach, stop/start, and suspend/resume recipe, see
[seat lifecycle](../../seat-lifecycle.md#find-a-workspace-and-return-to-an-interactive-seat).

### Applied subject definitions

`GET /v1/client/subject-definition?subject=agent%2Fexample%2Fworker` reads exactly one agent's
applied desired declaration, including mission-owned and ad-hoc seats. It requires
`read.projections`; the Rust method is
`Client::subject_definition(subject, show_env_values)`, returning `Envelope<SubjectDefinition>`
over either transport. Swift and TypeScript expose `subjectDefinition` with `showEnvValues`
defaulting to `false`.

Environment variable names are preserved, but their values are `"<redacted>"` by default in both
`desired` and `kdl`. Request `&show_env_values=true` to include literal values; that also requires
`read.declarations`, so a projection-only reader gets `forbidden` rather than secrets.

The value contains `kind: "subject-definition"`, `subject`, the typed canonical node tree
`desired` (`name`, optional positional `arguments`, sorted `properties`, ordered `children`),
rendered canonical KDL `kdl`, `desired_revision`, the selected desired claim `desired_token`,
and competing claim tokens in `conflicts`. The envelope snapshot's `store_index` fences all these
fields to one SQLite read snapshot. The read never includes the subject's claim history or other
subjects' definitions.

The KDL document starts with `version 2` and reconstructs the applied AST. It is suitable for
display without client-side KDL parsing. It is not original source: comments, whitespace, authored
entry ordering, and source paths are not retained. Clients label it, for example,
`applied · rev <desired_revision> · reconstructed`. A redacted document is not an applicable
copy of the definition: re-publishing it would replace the environment values with `<redacted>`.

Unknown agents and agents with observations but no applied desired declaration return typed
`not-found`. Other subject kinds return `validation-failed`: a mission is published as a compiled
revision (read it through `/missions/{id}`) and keeps no canonical declaration AST to render.
A definition is never truncated:
when its serialized value exceeds `max_response_bytes - 4096` (reserving room for the envelope),
the server returns `validation-failed` rather than an incomplete AST or KDL document.

### Usage over a period

`GET /v1/client/usage?since_ms=…&until_ms=…` reads token spend over a period: the last 24 hours
when both are omitted, ending now when `until_ms` is omitted. It requires `read.projections`. The
value is `UsagePeriod { since_ms, until_ms, rows }`, with one `UsageRow` per agent, mission run,
step, model, paying account and host, largest first. A row carries the token counts (`total`,
`input`, `output`, `cache_write`, `cache_write_1h`, `cached`), `cost_microusd` (API-equivalent
cost: the harness's own figure when it reports one, else st's pricing table, named by `pricing`),
`reported_cost_microusd` (the part the harness reported) and `unpriced_tokens` (tokens neither
could price, which a client shows as unknown cost, never as free). An identity st does not know,
such as the mission run of a standing seat, is absent from the row. A period whose start is after
its end is `validation-failed`. `limits` lists each account's selected limits reading: `account`
(a label such as `claude/<digest>`, or `DRIVER/unknown`), `driver`, optional `plan`,
`five_hour_percent`, `weekly_percent` and their `*_resets_at_unix_ms`, when and by which seat and
host it was measured, and the seats whose newest reading names the account. A harness that does
not report a value leaves it out. Selection uses quota observation time, independently of
context refresh and publication time. Within an hour of the account's freshest observation,
the highest weekly reading in the latest reported reset window wins; ties use source time
and seat identity. A new reset window can lower the reading, and an older high reading
outside the hour cannot override it. This same selection serves `st usage`, stui, iOS,
account pools and the limits policy. The policy tests freshness against the selected source
time, so a recent low publication cannot freshen an old high observation. Claim kinds and
client fields remain compatible with older clients.
Partial reports without a weekly percentage do not replace or refresh a prior weekly source.
The durable reading survives member restarts. Consumers must check its original measurement time
and reset window; missing or stale evidence is unknown, never zero. `st doctor` reports missing,
stale, future-dated or already-reset weekly evidence for active accounts as `account-limits`.
See [account limits](../accounts.md#at-the-limit) for the policy and external backstop behavior.
The Rust method is `Client::usage_period(since_ms, until_ms)`;
Swift has `usagePeriod(sinceMS:untilMS:)` and TypeScript `usagePeriod({ since_ms, until_ms })`.

### Clients connected now

A client may name itself and its build in an `x-st3-client` header on every request and stream,
for example `stui 0.1.0+a0c135e3`, `smalltalk-ios 1.0 (42)` or `st 0.1.0+a0c135e3`. The Rust
crate sends what `st3_client::set_client_name` was given; the TypeScript client takes `client` in
its options, and Swift `client:` in its initializer. The header is optional, and st shows it as
reported: it is never identity or authority, and no client is refused, slowed or warned for its
version.

Each member keeps, in memory only, the clients connected to it now: every open collection,
conversation or terminal stream, and every client that made a request in the last five minutes.
`GET /v1/client/clients` (`clients.list`, which requires `read.projections`) reads them as
`ClientConnections { kind: "client-connections", member, items }`, newest first. Each
`ClientConnection` has the acting session (`actor`), the person or agent whose authority it
uses (`person`), the reported `client`, the paired `device_id` and `device_name` when a paired
device's session is acting, the `member` it reached, `via` (`local` for the member's own Unix
socket; `gateway` for a paired device through the client gateway, which a Tailscale forward and
Fabric both reach, so st cannot tell them apart; `tailscale` when Tailscale Serve names itself),
whether it is `connected` now with how many open `streams`, `since` and `last_seen`, and what it
`follows`: windows by name, `conversation:ID` and `terminal:ID`. Another member's clients are on
that member's list. The Rust method is `Client::clients_list()`, Swift `clientsList()` and
TypeScript `clientsList()`. `st clients` (or `--json`) prints the list, and `st devices ls` adds
"connected now" or "last seen" with the reported build to each paired device.


`operations` is the client-safe operational view: daemon health, host reachability, transport
health, resource observers, and diagnostics. Some diagnostics compare the whole projection with
the claim log, so pages serve the daemon's last diagnostic report and a read of a report older
than 30 seconds starts a new one in the background. The daemon makes its first report in the
background as it starts; until that report is made, the collection lists one `running` operation,
`operation/diagnostic-report`, that says so.
`history` is a typed audit projection. It does not expose raw claims, replication envelopes, or
repair internals.

Attention is a read-only snapshot of current sources. Its identity is the source, recipient, and
waiting episode. `source_kind`, `episode`, `source_id`, `priority`, `requested_at`, and
`action_parameters` describe the source and its current remedy. Completed, cancelled, removed,
retired, or replaced sources disappear before cleanup; a failed run can retain its own fault.
Pending held subscription requests are not attention sources. Historical `attention.*` claims
remain audit data. Both raw legacy mutation routes and `attention.resolve` return
`attention-migrated`; capabilities mark that action unsupported.

An agent asks through `work.ask` (`person_id`, `title`, `reason`, and exactly one of `step_id` or
`new_run`). Claimed work requires its current generation, definition, attempt, readiness, and
incarnation fences. The ask creates a ready person-assigned runtime step and pauses its origin
in `waiting-person`, with no lease, timeout, or retry consumption. A named small run requires a
live requester declaration or owning run and rejects ambiguous claimed work. Repeating the same
ask key returns the same step. Retirement and generation replacement invalidate the ask.

`work.ask` may also carry a `request`: a `StructuredRequest` decision, choice or feedback with
named answers (see [the runtime guide](../mission-graph-runtime.md#structured-requests)). The
person-step attention card then includes the same `request`; a card without one is a free-text
ask, and clients should not infer answers from its title. A card with `update` instead brings
information the person asked for and asks nothing: show it, and send `work.done` with
`answer: {"id": "read"}` (its `action_parameters` already carry it) when the person opens it
or presses read. Agents post updates with `st work update`.

`work.done` takes `target_id`, `episode`, nonempty `summary`, optional string `evidence`, and
an optional `answer` (`id` and/or `text`). A structured decision or choice needs `answer.id`,
or text for an allowed custom choice; requesting changes and feedback need text. Validation
failures return `validation-failed` with the answer IDs in the message. The asker reads the
typed answer from the `work` resource's `person_answers`.
Only the assigned person or a session explicitly delegated by that person completes it. The
requester may instead use `work.cancel-ask`. Completion resumes a live origin in the same attempt
with a new readiness epoch; the response and evidence stay on the source. CLI equivalents are
`st work ask --for PERSON --title TEXT --reason TEXT --step STEP --as AGENT --idempotency-key KEY`
(or `--new-run NAME`) and `st work done STEP --as PERSON --summary TEXT`.

A `human-gate` card answers its gate with `review.approve` and, by its `review_mode`, either
`review.reject` (`approve`) or `review.request-changes` (`feedback`); the other is refused.
Each takes `target_id` set to the card's `source_id` (the step, mission or loop run that owns the
gate) and an optional `reason`, which the reject and request-changes actions require, and fences
the card's ID and revision. A gate asked again (a retried step's new attempt, or a newer build's
wording) gets a new card, so an action through the old one returns `stale-fence` and writes
nothing. A client then re-reads attention and, if a card of the same `attention_kind` for the
same `source_id` is open, acts once more through it; it does not retry the old card. A review st
no longer asks returns `validation-failed`, and its message says why: who already answered it
and how, or what moved on since it was asked. stui does both on its live and classic screens.

Clients must evict removed source cards and replace their window from fresh snapshots on
reconnect. The iOS cache version is 4 and stui's is 3; older cached cards are discarded. Offline
cards are marked stale and cannot submit actions. A future notification consumer should compare
fixed-recipient snapshots at an explicit `as_of` and deduplicate transitions by source, person,
and episode, notifying only when an episode first appears. There is no push delivery service.

Every attention resource carries its concrete `person_id`, original `source_id`, semantic
`attention_kind`, optional mission/run/step context, and currently meaningful typed actions. A
client can therefore render a mixed inbox, navigate to the source, and act without recovering
identity or graph context from prose.

A `fault` also carries `target_states`: for each target with a lifecycle (a mission, run,
generation, step, or agent), its current `state` and, when known, the `since`
time it entered that state. Resource and document targets have none. The card describes the
current failure and offers source inspection; recovery, cancellation, or retirement removes it.

Each agent resource includes `current_work_ids` and an ordered `upcoming_work_ids` preview across
mission runs. `next_work_id` is the first ready item, even while another step occupies the agent's
work seat. `active_work_count` and `queued_work_count` give complete counts; the ID lists include
at most five items each. Ready work follows the agent's seat queue: mission runs in queue order,
then step creation time and subject ID inside one run. These fields describe the queue and do not
imply that an active claim is making progress.

A trusted local agent session (`--as agent/PATH` on the local Unix API) holds every scope a person's
local session holds and may invoke every action, recorded with the agent as its actor; see
[free mode](../kdl-lifecycle.md#free-mode). Glasses stay a person's own: an agent session reads and
writes none. Each action keeps its own fences, and a review still needs its named reviewer.

`GET /v1/client/agent-queues/{agent_id}` returns one `AgentQueue` value for a seat: its
`current_work_ids`, its `next_work_id`, each queued mission run in order with `position`, `state`
(`claimed`, `ready`, or `waiting`), the run's own state, its join time, and its claimed, ready, and
waiting step IDs, and then the recent moves, newest first, with `move_count` for the full history.
Each move names its run, placement, optional anchor run, actor, optional reason, and time. A run
joins the queue when it first has a step assigned to the seat and leaves when it is terminal. An
unknown agent returns `not-found`.

A lane is one ordered line of entries that a mission run works through front first, such as a merge
train of pull requests. `/v1/client/lanes` lists open lanes; `history=true` adds lanes whose run
ended. Each `Lane` resource names its `mission_run_id`, `mission_id`, optional `entries_prefix`
and `approver_id`, and `state` (`open` or `closed`). `entries` are in lane order: each has its
`entry_id`, a short `label` without the prefix, `position`, the status the run recorded (`waiting`,
`held`, `ready`, or `running`) with its `detail`, exact `head`, marker, and time, who joined it and
when, and who approved it. `recent` lists joins, leaves, moves, and approvals, newest first. The
`st missions tree` view carries the open lanes as `lanes`. [Lanes](../lanes.md) explains the model.
The tree lists at most 200 active runs, steps per run, unstarted missions and agents, and standing
queues up to 1000 queued runs. When a fleet has more, `truncated` names each part that was cut
with how many it `shown` of the `total`.

Observer and subscription lists and details are available at `/v1/client/observers` and
`/v1/client/subscriptions`. Each resource includes its normalized specification, current state,
and owning run, generation, and step. Agentless work includes `gate_kind`: `watch` for a standing
step with no gate, `predicate`, `command`, `llm`, or `human` for a single gate family, `mixed`
for combined families, and `run` for an `after-run` step.

## Harness-neutral session timeline

The timeline schema deliberately contains no Claude, Codex, Pi, OMP, or transcript-file types. A
session has ordered entries with a strictly increasing `sequence`, stable entry ID, RFC 3339
timestamp, `role` (`system`, `user`, `assistant`, or `tool`), and one typed body:

- `message`: logical turn metadata;
- `content`: text or an attachment reference with a media type;
- `tool_call`: stable call ID, tool name, and JSON arguments;
- `tool_result`: the matching call ID, JSON/text content, and success status;
- `status`: queued, running, waiting, completed, failed, or cancelled;
- `error`: versioned safe code, message, retryability, and details;
- `usage`: input, output, cached, and total tokens plus optional cost data;
- `redaction`: reason and the byte or item count withheld;
- `truncation`: omitted range, reason, and a continuation cursor when recoverable.

Incremental timeline events use `append`, `replace`, or `finalize`. `replace` targets an existing
entry and increments its revision; it cannot change the entry's ID, sequence, role, or type.
`finalize` makes the entry immutable. Tool results must refer to a preceding tool call. Timeline
pages and updates are bounded by the negotiated byte and item limits.

Pi-family native replay recognizes OMP's message-level `role: "toolResult"` records: the
`toolCallId` correlates the result with its call, `isError` selects error or success status, and
the result's text content is retained. Legacy tool-result blocks inside message content remain
supported.

External process sessions remain listed even when st cannot identify a native transcript.
Opening their timeline returns a non-retryable `unsupported-capability` error with
`details.reason: native-session-unidentified` and `details.session_id`, explaining that the
agent was not started by st and its saved session could not be identified. Clients show this
as the no-conversation state, rather than treating the listed session as missing. The same
verdict applies to the conversation stream. OMP `__omp_worker_*` internal modes are helpers,
not external harness sessions, and are excluded from discovery.

## Launches, variants, decisions, and approvals

`launch` is the only user-facing noun for authoring and reviewing a mission. Planning remains an
internal phase and is not an API resource or a CLI compatibility alias.

A launch owns its request, target (new mission or an exact mission run generation), variants,
decisions, approvals, and terminal outcome. Variant content is typed projected mission data; clients
do not submit or receive KDL or Markdown. A preview returns its normalized graph, validation
diagnostics, and a deterministic token:

Launch creation may select an eligible planner provider, model, and effort. The server applies its
configured default only to new launches and records the effective immutable `planner_config` with
the launch and its planner agent. Selecting a different provider without model or effort overrides
does not carry the old provider's defaults into that new session.

```
lpv0:<lowercase SHA-256 of RFC 8785 canonical JSON {
  api_version, launch_id, variant_id, candidate_revision,
  target_generation, normalized_mission, diagnostics
}>
```

The same values always produce the same token on every host. Approval carries that exact token and
the launch revision fence. Any candidate, target generation, normalized mission, or diagnostic
change produces another token. Approval publishes a mission revision but does not start it.

Every preview also carries `structured_diff` and a `st3.visualization.v0` model. The model has
graph nodes and edges, timeline entries, assignment swimlanes, revision and risk summaries, and
live-progress placeholders with explicit attempts, leases, progress, blockers, attention, errors,
and cursors. It is the shared input to graphical and textual clients; those clients never recover
structure from prose.

## Typed actions and fences

All mutations use `POST /v1/client/actions` and the `ActionRequest` union in the schema. The common
fields are:

- `id`: a client-generated stable action ID;
- `type`: the action discriminator;
- `idempotency_key`: unique within the paired session for at least 30 days;
- `fence`: the snapshot and exact mutable identities the user acted on;
- `parameters`: the type-specific body.

The authenticated session supplies actor and scopes. `actor`, `credential`, and fleet secrets are
invalid request fields. Repeating the same key, action ID, type and parameters returns the
original result; a refreshed fence does not change the action's idempotency identity. A different
action under that key returns `idempotency-conflict`. Receipts from older daemons still accept the
exact original request. A snapshot fence proves the host and an index no newer than the current
store; unrelated commits do not stale an action. Stale generation, subject revision, incarnation,
preview or terminal screen fences return `stale-fence` without a partial mutation.
Multi-subject actions commit atomically or have no effect.

The v0 action discriminators are:

| Family | Actions | Required fences |
|---|---|---|
| Attention | `work.done`, `review.approve`, `review.reject`, `review.request-changes` | source episode or review revision |
| Messages | `message.send`, `message.read`, `message.close` | reply/message revision when present |
| Launches | `launch.create`, `launch.revise`, `launch.preview`, `launch.approve`, `launch.cancel` | launch revision; target generation and preview token where applicable |
| Missions | `mission.start`, `mission.revise`, `mission.approve-revision`, `mission.cancel-revision`, `mission.cancel` | mission revision and current generation where applicable |
| Sessions | `session.import` | exact native-session revision; an exact running-process fingerprint is revalidated server-side |
| Work | `work.ask`, `work.cancel-ask`, `work.claim`, `work.renew`, `work.progress`, `work.complete`, `work.fail`, `work.release`, `work.retry`, `work.publish-mission` | generation, definition, attempt, readiness epoch, and claimant incarnation after claim |
| Seat queues | `agent.queue-move` | snapshot; the run and any anchor run must be queued for the seat |
| Lanes | `lane.join`, `lane.leave`, `lane.move`, `lane.mark`, `lane.approve` | snapshot; the lane must be open and a named entry or anchor must be in it |
| Runtimes | `runtime.stop`, `runtime.restart`, `runtime.reset`, `runtime.context-clear`, `runtime.signal` | runtime incarnation; stop, restart, and reset also require `runtime_desired_revision` from the runtime resource |
| Agent desired state | `agent.stop`, `agent.start` | snapshot and `runtime_desired_revision`, the agent's selected desired claim ID; no runtime incarnation required |
| Terminals | `terminal.input`, `terminal.resize`, `terminal.attach`, `terminal.detach` | runtime incarnation; input and resize also require the screen sequence |
| Pairing | `pairing.begin`, `pairing.complete`, `pairing.revoke` | pairing/device revision where applicable |

`runtime.stop` publishes a stop for the selected member. `runtime.restart` terminates the current
incarnation of a member with an `always` restart policy; its desired state then starts the next
incarnation. `runtime.reset` publishes a restart-window reset for a run-owned member. A client
submits the runtime resource ID as `target_id` and copies its `incarnation_id` and
`desired_revision` into the action fence. Runtimes with no selected desired state have a null
`desired_revision` and cannot use these controls.

`agent.stop` takes `{ "agent": "agent/NAME", "reason": "optional explanation" }`;
`agent.start` takes `{ "agent": "agent/NAME" }`. These require `control.runtimes` and the
same authority as `agent.create`: the session's concrete person, or a local agent acting as
itself (free mode).
Stop publishes the same desired stop as `st agents stop`, even if no runtime is live, and
preserves the immutable declaration. Start restores the unambiguous preceding agent
declaration with its original identity and host, like `st agents start` without overrides.
Mission-owned agents must instead be changed through their mission. Both actions copy the
selected desired claim ID into `fence.runtime_desired_revision`, reject stale fences, and
use the normal audited action receipt/idempotency key; exact retries return the saved result.
Rust exposes `agent_stop`/`agent_start`; TypeScript and Swift expose `agentStop`/`agentStart`.
All generated clients' `Fence` models also carry the desired revision required by
`runtime.stop` and `runtime.restart`.

`agent.queue-move` takes `agent_id`, `mission_run_id`, `placement` (`top`, `bottom`, `before`, or
`after`), `anchor_run_id` for `before` and `after`, and an optional `reason`. It records one
`agent.queue.moved` claim with the session's person as actor. It never changes a step the seat
already holds. A run or anchor that is not queued for the seat returns `validation-failed`.

The lane actions take `lane_id` and `entry_id`. `lane.join` and `lane.approve` take an optional
`reason`; `lane.leave` takes an optional `outcome` (`completed` or `removed`, default `removed`) and
`reason`; `lane.move` takes `placement` and, for `before` and `after`, `anchor_id`; `lane.mark` takes
`state` and optional `detail` and `head`. Each records one `lane.*` claim with the session's person
as actor, and affects the lane's ID. A join of an entry already in the lane records nothing. Only
the lane's `approver_id` can approve (`forbidden` otherwise). An entry or anchor that is not in the
lane, or a closed lane, returns `validation-failed`.

An accepted action returns one stable operation ID and status. `202 accepted` means the command is
durable, not complete; clients follow operation events or read `/operations/{id}`. Result objects
include affected stable IDs and the resulting snapshot ID.

## Attachments

A `message.send` action may carry `attachments: [{blob, media_type, name?}]`, up to four, and a
message resource lists `attachments: [{blob, sha256, media_type, name, size, origin}]`. The bytes
are not in the graph. [Attachments](../attachments.md) defines the limits, retention and delivery.

- `POST /v1/client/blobs`: raw image body, `Content-Type` one of `image/png`, `image/jpeg`,
  `image/gif`, `image/webp`, scope `control.messages`; answers `{blob, sha256, size, media_type}`.
- `GET /v1/client/blobs/{sha256}?message=message/ID`: the raw bytes.
- `GET /v1/client/blobs/{sha256}/chunk?message=message/ID&offset=N`: up to 512 KiB as base64 JSON,
  with the total `size`. `Client::blob` / `blob()` in each client assembles the file.
- Errors: `blob-too-large`, `unsupported-media-type`, `blob-content-mismatch`,
  `blob-quota-exceeded`, `blob-not-found`, `blob-expired`.

## Event feed and resynchronization

`GET /v1/client/events?after=CURSOR&limit=N&wait_ms=M` returns at most the negotiated event and byte
limits. `wait_ms` is clamped and is only a bounded long poll. The opaque cursor denotes the next
projection event, not a graph or replication position. Events are ordered by `(epoch, sequence)` and
contain a unique ID, previous and next cursor, timestamp, `upsert`, `delete`, `timeline.delta`,
`terminal.available`, or `capabilities.changed`, affected resource IDs, and the resulting snapshot
ID. Replaying `after` is safe and may repeat the last page; clients deduplicate by event ID.

The response advertises `oldest_cursor` and `resume_cursor`. If `after` is unknown, expired, belongs
to another authenticated scope, or precedes retention, the server returns the versioned
`cursor-gap` error with `full_resync: true`. The client discards projection caches, fetches fresh
snapshot pages, and resumes from the capabilities response's `event_cursor`. It must not infer
missing mutations or request graph replication.

A timeline whose retained claims omit an entry's append, or omit older history without a typed
truncation interval, reports `timeline-history-incomplete` with `retryable: false`, `full_resync: false` and
`retained_history_incomplete: true`. Its message explains that the retained transcript start is
incomplete. Retrying a fresh snapshot cannot restore those missing claims; clients show the
reason and stop automatic retries. Ordinary expired stream cursors remain retryable.

## Pairing and remote access

Pairing is device-to-person. Its begin request names the concrete initiating `person/*` identity and
is accepted only on the trusted local Unix API; the daemon persists that person as the delegator.
The response shows a short-lived single-use code and pairing ID. A remote device reaches the
loopback gateway through Fabric, proves the code, supplies its public key, and receives a scoped
credential bound to that key. The resulting session returns the exact delegated person, its derived
device-session actor, and granted scopes.
Pairing codes expire after five minutes and reveal no fleet secret. The remote device cannot
request its own actor or scopes. By default the trusted local begin grants projection reads,
glasses reads and control, terminal reads, attention control, and launch control. The begin
request may narrow that default with `scopes`, a non-empty subset of exactly those limited
scopes (`read.projections`, `read.glasses`, `control.glasses`, `terminal.read`,
`control.attention`, `control.launches`); any other scope, or `scopes` combined with
`full_control`, is rejected with a validation error. `st devices --as person/alex pair
--read-only "Wall display"` requests `read.projections`, `read.glasses`, and `terminal.read`, so
the device can observe but every attention, launch, and other action returns `403`. For an
intentionally trusted device that needs Chat sends, mission/work actions, runtime control, and
terminal input, the initiating person
must use `st devices --as person/alex pair --full-control "Alex iPhone"` on the trusted local
socket. The selected concrete scopes are sealed into that pairing; existing limited devices are
not silently upgraded and must be re-paired, then revoked when no longer needed. Revocation takes
effect for every subsequent request, including a new bounded terminal WebSocket exchange.

Agent declarations are a separate, sensitive read: `GET /v1/client/agent-declarations/{id}`
returns the currently applied desired tree, its canonical KDL v2 text, exact revision ID, and
the immutable revision IDs newest-first. `?revision=ID` selects only that exact agent claim,
including superseded declarations; an unknown revision or unmanaged session returns 404.
Environment variable names are preserved, but their values are `"<redacted>"` by default
in both the desired tree and KDL. Explicitly request `?show_env_values=true` (combined with
`&revision=ID` for a past revision) to include literal environment values. Both current and
historical bodies, redacted or not, require `read.declarations`; `read.projections` alone
does not authorize this endpoint. Default limited pairing never grants `read.declarations`.
Full-control pairing does, so grant it only to a trusted device whose holder may explicitly
inspect declaration secrets; revoke or re-pair existing devices to change their sealed scopes.
Likewise, `st subject show agent/NAME --kdl` redacts environment values; add
`--show-env-values` to include them. KDL is a normalized representation of the applied
desired state, not a recovery of authored whitespace or comments.

Read-only scope permits snapshots, details, timelines, and event feeds. `terminal.control` adds
terminal input and resize; other control scopes are action-family-specific. A capabilities response
must distinguish unavailable, ungranted, and unsupported features.

## Conversation stream

`GET /v1/client/conversations/{id}/stream` opens one authenticated WebSocket
with subprotocol `st3.client.conversation.v0`. A client may pass `after=CURSOR` to
resume. `{id}` may be a session ID, an agent peer ID (resolved to its current
session), or a st message ID with a session peer. A cursor remains tied to the
resolved session, so a new agent incarnation needs a fresh stream.
The first envelope has a `ConversationChanges` value with an empty `items`
array and a `next_cursor` when opening at the live edge. Later envelopes contain
new chronological `TimelineEntry` values, including st messages, and a cursor to
save after applying the batch. The owner sends no WebSocket data while idle.

The gateway routes managed sessions to their owning host using the authenticated
daemon relay. The owner holds a bounded change read for up to ten seconds. A
reconnect replays at most 200 entries; an older cursor returns `cursor-gap`, so
the client must reload the timeline before reopening. Cursors belong to one
session and one owner. `GET /v1/client/conversations/{id}/changes?after=CURSOR&wait_ms=N`
offers the same bounded change read for clients that cannot open WebSockets.

## Terminal protocol

The projected terminal protocol sends complete screens: each screen replaces every earlier one,
so a client that falls behind skips to the latest screen. Full-fidelity applications use the raw
PTY transport below instead. Interactive attach from a terminal on the owning host
(`pty attach`, `st terminals attach`) is a different, privileged path that passes raw bytes. On that
host, `st terminals attach` reads the PTY session from the local daemon
(`GET /v1/sessions/local-terminal/{subject}`, which writes nothing) and connects to that session
itself. Before it sends a byte, the socket's kernel-reported peer and the PTY record must match the
runtime incarnation. Through an HTTP endpoint, or with a daemon that lacks that route, it uses the
daemon's WebSocket bridge with a single-use capability. The Fabric-loopback gateway refuses the
local-terminal route, as it refuses every route outside `/v1/client/`. When the subject has a
running PTY session under this host's configured PTY root and the daemon, at whichever `--endpoint`
was given, does not answer within a second, the CLI attaches to the newest of those sessions without
it, as the local user. It prints that st was not consulted, and the kernel-reported peer must still
be the PTY daemon the registry records. With no such session it says, after that second, which
daemon it is still waiting for.

`st terminals attach` to a terminal on another fleet host first tries the same raw path over Fabric,
as the configured person. The CLI checks that the gateway grants that person `terminal.read` and
`terminal.control`, and reads the owner, PTY session, and runtime incarnation from its local daemon.
It then dials the owner's Fabric NodeID, which the owner's member record advertises or, in a
config-peer fleet, the one trusted Fabric peer with the owner's name, ignoring case. The protocol is
`st3/pty/FLEET_ID`, which `st terminals expose-fabric` has the owner's Fabric serve by running
`st terminals serve-fabric --stdio` for each tunnel, so no st daemon on either host takes part. The
CLI sends one line, the `pty remote-serve` route line plus the subject and incarnation:
`{"op":"route","name":RUNTIME_ID,"subject":SUBJECT,"incarnation":INCARNATION}`. The owner refuses
a name that is not one file under its PTY root and a session that is not tagged as the subject's.
It refuses a session whose kernel-reported socket peer and PTY record do not match the incarnation.
Otherwise it answers `{"ok":true}` and splices the tunnel to the session socket. After a lost
tunnel the CLI dials again with the same incarnation until the owner refuses. When Fabric cannot
reach the owner or the owner refuses, the CLI says why and falls back to this protocol. It paints
each screen into the local terminal and sends keystrokes and size changes as `terminal.input` (raw
mode) and `terminal.resize` actions.

`terminal.attach` returns a stream capability and URL bound to the authenticated session, terminal,
and runtime incarnation; `terminal.detach` idempotently invalidates that viewer. The capability is a
lease: `reusable` is true, `ttl_s` (300) and `expires_at` say how long it lives, and every stream a
client opens with it before then is accepted, so a reconnecting client reuses it instead of
attaching again and, for a remote terminal, instead of making the gateway ask the owner again. The
lease ends at `expires_at`, at `terminal.detach`, or when the runtime incarnation changes; the
attachment then reports `state` `expired` or `detached` with no capability and `retry_hint`
`reattach`: attach again with a new idempotency key. A stream already open is not cut off when its
lease expires. A client opens the URL on the same Unix or Fabric-loopback gateway with WebSocket
subprotocol `st3.client.terminal.v0`. Authentication, capability validation, and
runtime-incarnation validation happen before upgrade.

The WebSocket then stays open. The first message is the current screen. After that the server sends
a new screen only when the screen changes: the first change after a quiet period at once, and later
changes at most every 100 ms. An idle terminal sends nothing, including no keepalive; a client that
needs one sends WebSocket pings, which the server answers. A slow client receives the latest screen
when it can read again, not every screen it missed. Every message is an envelope whose value is a
`TerminalScreen`, the same value `GET /v1/client/terminals/{id}/screen` returns.

A screen carries the runtime incarnation, `rows` and `columns`, the cursor (`row`, `column`,
`visible`, `style` of `block`, `underline`, or `bar`, and `blinking`), the title, the input `modes` a
client needs to encode keys and pastes (`alternate_screen`, `application_cursor`,
`application_keypad`, `bracketed_paste`, `focus_events`, `mouse_tracking`, `mouse_encoding`), one
line per row, and `next_sequence`, an opaque numeric screen fence for input and resize. Compare
it for equality; it is not a graph index or an ordered event counter. Unrelated graph writes do
not change it. A terminal action may use an older snapshot from the same host, while incarnation
and explicit revision fences still apply. Attach/detach do not require a screen sequence fence.
`revision` digests the rest of
the screen: equal revisions mean equal screens, and a stream never sends the same revision twice.
The optional `kitty_keyboard` mode carries the active Kitty keyboard enhancement bitmask.

Each line keeps its plain `text`, without trailing spaces, and adds `runs`: styled text from column
zero. A run has `text` and, when they differ from the terminal default, `fg` and `bg` colors and
`bold`, `dim`, `italic`, `underline`, and `inverse` flags that are present only when set. A color is
a palette index from 0 to 255, where 0 to 15 are the client's ANSI theme colors, or a `#rrggbb`
string. The runs spell `text`, followed by any trailing blanks that are visible because of their
background, inverse, or underline. Hidden text is sent as spaces.
Optional line `wrapped` marks automatic continuation to the next row. Optional run `cells`
records display width rather than text length; `strikethrough` is present when set, and `link`
contains the hyperlink `uri`. Generated Rust, Swift, and TypeScript models preserve these fields.

When the terminal's runtime incarnation changes or its process exits, the server sends one
`stale-fence` error envelope and closes the stream, with the error code as the close reason. Other
failures end the same way with their own code. A client reconnects with a fresh `terminal.attach`,
and its first message is again the current screen. Input and resize remain fenced typed actions.

A gateway relays a terminal that another host owns through bounded owner long polls:
`GET /v1/client/terminals/{id}/screen?after=REVISION&wait_ms=N` returns as soon as the screen's
revision differs from `after`, or the current screen after `wait_ms` (at most 30000). The owner
follows its PTY and answers the moment the screen changes; an idle remote terminal costs one relay
request per 10-second wait and still sends the client nothing.

The owner does not have to be the gateway's peer. Every read of another host's conversation or
terminal, and every terminal control, goes to the owner directly when the gateway can dial it, and
otherwise to the peer nearest the owner by the fleet's replicated transport observations. When no
node has observed the owner, each peer is tried in turn. Each node on the way forwards the read the
same way, at most four times and never through a node it already passed, and relays the owner's
answer or refusal back unchanged. Every hop checks that its sender is a fleet member, and the owner
applies its own grants to the person the read carries. A laptop peered only with a desktop
therefore reads a conversation on a server that only the desktop dials. Each hop waits longer than
the next one, so a long poll's answer is never cut short on its way back.

`GET /v1/client/messages?actor=AGENT` lists an agent that another host owns from that host: the
gateway relays the read for a concrete person or agent, the owner lists the messages it holds, and
the page carries `replicated` with `source` `owner`, `complete` true and `state` `current`. Messages
reach a gateway by replication, so its own copy can lag and read as empty. When the owner cannot be
asked, the gateway returns its replica, never as if it were whole: `replicated` has `source`
`replica`, `complete` false, `state` `lagging` (this host is still catching up with the owner's
fleet, see `sync`) or `unverified`, and the `reason` the owner was not asked, such as `no-route` or
`timed-out`. A client renders that as waiting, not as no messages. A list that names no remote
agent has no `replicated`. Page cursors of a relayed list belong to the owner.

A read that no peer can carry fails with `remote-unavailable`, and its `details` say why, so a
client can tell a host nobody reaches from a slow or refusing one: `reason` is `no-route` (this node
cannot dial the owner and no peer reaches it), `dial-failed`, `timed-out`, `refused` (a peer does
not accept this node's reads), `hop-limit`, `owner-error` or `transport-error`; `owner_host_id`
names the owner; `hops` counts the nodes the furthest route handed the read to; `attempts` lists
each next hop tried with its own `reason`; and `elapsed_ms` is how long the gateway spent. An
owner's own refusal, such as `stale-fence`, carries `owner_host_id`, `elapsed_ms` and
`fence_conflicts` (how many of this gateway's reads that owner refused as stale in the last
minute). The gateway logs one warning per unreachable owner with the same fields.

`GET /v1/client/terminals/{id}/screen` answers a remote terminal with a `relay` object beside the
screen: `owner_host_id`, `via` (the first node the gateway sent the read to), `direct` (that node
is the owner), `transport` (`fabric` or `http`), `rtt_ms` (the gateway's wait for the owner),
`capability_ttl_s` (how long an attach capability from this gateway lives) and `fence_conflicts`.
It is provenance, not authority: it never enters `revision`, and a screen from the owner's own host
has none.

With `?facts=true`, the owner also returns best-effort `facts` read from the session itself:
`rows`, `columns`, `clients` (`total`, `attached`, `read_only`), `process` (`alive`, `exit_code`),
`uptime_s` and `tags`. A remote screen carries the owner's facts, so one read fills a fleet
client's Session view. Facts are left out, not an error, when the session does not answer within a
second; they stay out of `revision`, appear only on this read and never on a stream's frames, and
no action's admission depends on them.

Read-only terminal scope permits screens but rejects input and resize. Screen payloads obey
negotiated byte limits: at most 200 lines and 4096 bytes of text per line, with explicit
`redacted` and `truncated` markers.

### Raw PTY transport

`POST /v1/client/terminals/{id}/raw-attachments` accepts
`{"runtime_incarnation":"PID:CREATED_AT","mode":"attach"}` (or `"peek"`).
The ordinary client response envelope carries `terminal_id`, `runtime_incarnation`,
`owner_host_id`, `mode`, and `stream_capability` in `value`. A capability expires after 60 seconds
and can open exactly one transport. It is bound to the gateway's authenticated session and person,
terminal, owner host, runtime ID and incarnation, and access mode. Projected-screen capabilities
cannot open raw streams, and raw capabilities cannot open projected streams.

Open `/v1/client/terminals/{id}/raw-stream?incarnation=...&mode=attach` using WebSocket subprotocol
`st3.client.pty.v0` and secondary `st3.cap.CAPABILITY`. The credential and capability never appear
in the URL. Authentication, mode checking, single-use consumption, owner graph fencing and the
owner's kernel/registry incarnation proof all precede upgrade. Both modes require a concrete
person and `terminal.read`; `attach` also requires `terminal.control`.

Binary WebSocket messages are consecutive bytes of the original PTY protocol, not JSON screens.
Message boundaries have no PTY meaning. The client sends ATTACH (or PEEK) itself, and receives the
PTY's atomic SCREEN replay followed by live DATA, GEOMETRY and EXIT unchanged. The owner holds
one PTY connection for the transport lifetime. ATTACH and RESIZE therefore participate in normal
per-axis min-wins geometry with other persistent writers; PEEK cannot send input, resize, upgrade
to ATTACH, or contribute geometry. DETACH and closing the transport release the connection.
Raw clients cannot issue PTY lifecycle/CAS or ancestry-management commands through this capability.
Bounded chunks and socket backpressure preserve every byte; slow consumers do not skip output.

`st3-client::Client::raw_terminal_attachment` obtains the capability and
`raw_terminal_stream` returns a Tokio `UnixStream`. A terminal renderer can run its own PTY
parser, mode-aware paste, and geometry negotiation over it without learning about the fleet.
Each reconnect obtains a fresh capability for the same explicitly chosen incarnation; the
transport does not silently reselect a replacement runtime or replay input.

Remote raw transport chooses the owner's currently advertised direct member route, over signed
HTTP/WebSocket replication transport or optional Fabric. Modern membership-only fleets do not need
static config peers or Fabric. Every peer handshake verifies fleet authentication and current
member signatures; the owner then rechecks person authority and the exact live incarnation.
Unlike projected-screen reads, raw streams currently require a directly dialable owner endpoint;
they fail with `remote-unavailable` rather than replacing the stream with synthetic screens.
The owner's replication worker advertises its Tailscale endpoint, whether discovered or set with
a Tailscale `peer_listen`; see [Tailscale setup](../tailscale.md).
The client-facing carrier remains a forwarder to `st3-client.sock`, never `st3.sock`.


## Errors and evolution

Errors have `error_version: st3.client.error.v0`, a stable kebab-case code, safe message,
`retryable`, structured details, and optional `retry_after_ms`. Required v0 codes are `not-found`,
`forbidden`, `unsupported-capability`, `validation-failed`, `idempotency-conflict`, `stale-fence`,
`cursor-gap`, `page-cursor-expired`, `rate-limited`, `runtime-not-local`,
`runtime-authority-indeterminate`, `remote-unavailable`, `terminal-unavailable`, `terminal-ended`,
and `internal`.

Adding optional fields is compatible. Removing or retyping a field, changing ordering or token
rules, adding a required action parameter, or changing action semantics requires a new capability
version or API version. Clients preserve unknown enum cases for display but never send an action
whose capability version they do not understand.

### Inline schema semantics

The shared JSON Schema carries client semantics next to the wire constraints, not in a separate
registry. Standard Draft 2020-12 validators ignore these `x-st-*` annotations:

- `x-st-ref` names the subject family, a set of allowed families, or `*` for a generic subject.
  Named IDs retain the `family/non-whitespace-suffix` wire shape, including nested suffixes.
  Mission run-generation maps validate both mission-run keys and run-generation values.
- `x-st-brand` distinguishes opaque cursors, revisions, preview tokens, idempotency keys, and
  screen revisions. They remain strings on the wire; compare for equality and echo them unchanged.
- `x-st-codec` identifies RFC 3339 timestamps, Unix epoch milliseconds, millisecond/second
  durations, and redacted credentials. Units and credential sensitivity are explicit.
- Root `x-st-integers: "json-safe"` declares the safe-integer policy for generated JSON clients.
  Existing JSON Schema bounds still apply.

Evolving mission, agent, timeline, and error enums use `anyOf` with known values plus a string
branch. Consumers accept future strings without changing their wire value. `Resource` includes
`UnknownResource`, which preserves a future kind's valid header and arbitrary payload. Its kind
exclusion prevents malformed known resources from falling through that branch. The generated raw
Rust, Swift, and TypeScript `Resource` types model known kinds only, so TypeScript keeps
discriminant narrowing on `kind`.

Daemon conformance uses a strict test-only view of the same schema: it closes composed resource
fields, tightens open enums to known values, and removes `UnknownResource`. Consumer tolerance
does not authorize producers to emit undeclared cases or weaken family-ID validation.

## Conformance assets

[`schemas/client-v0.schema.json`](schemas/client-v0.schema.json) contains the shared wire types.
[`schemas/operations.json`](schemas/operations.json) is the machine-readable route/action/capability
manifest. [`fixtures/manifest.json`](fixtures/manifest.json) maps every golden fixture to its root
schema definition. The Rust tests validate fixture coverage, IDs, ordering, fences, timeline links,
and deterministic preview tokens. Daemon conformance tests validate synthetic collection,
conversation, and terminal frames with a Draft 2020-12 validator and reject undeclared resource
fields in the strict test view. No real user data is required for the conformance vectors.

## Private glasses

A glass is one person's named workspace. Its stable subject is `glass/person/NAME/UUID`;
clients generate a lowercase UUID (stui uses UUIDv7). Renaming changes `body.name`, never the
ID. Names are free text and need not be unique. The client handles name lookup.

`GET /v1/client/glasses` returns the ordinary paged resource list, ordered by ID, and
`GET /v1/client/glasses/{uuid}` returns one resource. The authenticated session determines
its person; these routes accept no owner selector. Anonymous sessions and agents have no glass
access. Paired devices need `read.glasses` for reads and `control.glasses` for writes. New
limited pairings include both grants. Existing devices with explicit grants need a new pairing
if they lack them. Discover the granted `glasses` capability (version 1 or later) before migrating local
storage; it is granted when the session has both read and write access. Version 1 stores
splits with tab groups; version 0 used tabs containing splits and is not compatible with this body.

`PUT /v1/client/glasses/{uuid}` accepts `{body, base_revision}`. A new ID requires a null
base revision. Existing IDs accept stale or null bases: writes replace the whole body, using
canonical claim order to choose the winner. `DELETE` on that route accepts `{base_revision}`
and records a tombstone. Both mutations require an `Idempotency-Key` header (1–200 bytes).
Reusing a key with identical input returns the same accepted revision; different input fails.
The device/session identity isolates keys. A deleted ID is permanently retired, including
when an offline device sends an edit after the deletion.

A resource contains `id`, `kind: "glass"`, `revision`, `updated_at`, `body`, `deleted`,
`base_revision`, and `replaced_revision`. Mutation responses identify the revision accepted by
this member; a subsequently received concurrent revision may win. `base_revision` records the
client's basis; `replaced_revision` records the head this member observed under its writer
transaction. Both are null on a first creation. A deletion response has a null body and
`deleted: true`; lists and detail reads show only current live glasses.

The structure is `{name, layout}`. A layout is a leaf group
`{tabs:[{title?, pane: "opaque key"}]}` or a binary split
`{split: "right" | "below", children: [layout, layout]}`. Each group has its own tab strip.
A split may include `ratio`, a number from 0.1 through 0.9 specifying the first child’s share;
absent means equal halves (0.5). Send it only to a member advertising the granted `glasses`
capability at version 2 or later. Version 1 bodies remain valid without a ratio. This optional
field changes neither depth nor node limits.
Pane keys convey no authority. Focus, the selected tab in each group, scroll, and
last-used glass stay local. Empty groups `{tabs: []}` are valid; clients supply their implicit
Home locally in the first group. Names and pane keys must be nonempty; splits have exactly
two children, and no unknown structure fields are accepted.
The daemon advertises limits: 65,536 bytes of compact UTF-8 JSON per body, 32 layout levels
(the root is level 1), 1,024 tabs and split nodes combined across the whole tree, and 100 live
glasses per person. Leaf group containers do not add nodes; tabs do not add layout depth.
Version-1 client writes reject the previous `{name, tabs}` body. Stored version-0 bodies
are projected as one tab group: each pane becomes a tab in its old left/top-to-right/bottom
order, and each old tab’s title goes to its first pane. The original claims and revisions remain unchanged, including during replication
and replay. A subsequent version-1 write stores the new body and records the old revision it
replaced. New client writes require the version-1 shape and bounds. An empty legacy body at
the old byte limit can project slightly above 64 KiB; the next write must fit the current limit.
The response ceiling is
8 MiB, allowing a complete 100-glass subscription window at these bounds.

Local creation is refused when the member already sees 100 live glasses. Concurrent creates
on separate members are all retained as immutable claims. After synchronization, the earliest
100 created, undeleted IDs in canonical claim order occupy the live slots; the remaining
bodies are retained outside the live view. Deleting a live glass opens a slot for the next
retained ID. Editing or renaming does not change creation priority. Detail reads outside the
live quota return `not-found`; a client that saves on a disconnected member may later see its
ID disappear from the live view after synchronization. This rule converges independently of
arrival order and never discards the saved structure.

Subscribe to `collection: "glasses"` on `st3.client.collections.v0`, with `limit: 100`, to
follow the person's current glass set. The existing `snapshot` / `changes` frames carry full
resource upserts and removed IDs, including deletions and quota changes. A reconnect starts
with an authoritative snapshot. The server applies ownership and read grants to each
subscription read. Glass bodies are excluded from generic claim lists, claim detail, status,
events, and history used by agents. Dedicated operations and replicated admission both check
the claim's person owner. Typed `glass.upserted` and `glass.deleted` claims are durable;
reads derive their answers from canonical order, and checkpoint proofs compare the same view.

Generated clients expose Rust `list_glasses`, `get_glass`, `put_glass`, `delete_glass`, and
`CollectionStream::subscribe_glasses`; TypeScript `listGlasses`, `getGlass`, `putGlass`,
`deleteGlass`, and `CollectionStream.subscribeGlasses`; and Swift `listGlasses`, `getGlass`,
`putGlass`, `deleteGlass`, and `glassesStream`. Each supplies typed bodies and recursive layouts.
Mutation methods take an explicit idempotency key so a retry uses the original key and input.
Member daemons replicate the claims; paired clients read them through a member gateway.

## Agent and plain-shell creation

`agent.create` takes `name`, `harness` (`claude`, `codex`, `omp`, `pi`, `opencode`), optional `host`,
`model`, `effort`, `workspace`, `repo`, `base`, `branch`, `remove_at_run_end`, `description`, and `message`. Names are stable seat identities as
in `st agents new`. Workspace paths are absolute on the selected host; omission asks that host for
its usual new-agent directory. The host creates missing agent workspaces. OpenCode does not accept
`effort`. Message text is nonempty, at most 64 KiB, and uses the native harness startup argument.
An existing active seat requires a different name; a stopped seat may be deliberately recreated.

`repo` is an existing repository's absolute path on the selected host. It adds the existing
`checkout` declaration and creates the agent workspace as a worktree. `base` defaults to
`origin/main`; `branch` defaults to the simple agent name with Git-invalid characters replaced
by dashes. An existing branch is reused. `base`, `branch`, and `remove_at_run_end` require `repo`.
An optional `workspace` chooses the worktree destination; otherwise the host names it as usual.
With `remove_at_run_end: true`, stopping a top-level seat removes its clean worktree after its
runtime stops. A mission seat removes it during run cleanup. The branch stays; changed or shared
worktrees stay with a diagnostic. A branch in another worktree reports that path and waits for
a changed declaration instead of repeatedly retrying Git. Missing repositories prevent launch
and produce an agent fault naming the path. An existing workspace must match the repository and
branch; a plain directory cannot bypass the checkout.

`GET /v1/client/hosts/{id}/repositories` (`host.repositories`) returns `HostRepositories`:
`host_id` and `repositories`, each with `path`, `workspaces`, and `agent_ids`. Any member can
answer for any host from replicated checkout declarations and latest `workspace.observed`
claims authored by that host. Reconciliation observes Git directories for plain workspaces;
reads never inspect another host's disk or scan the filesystem. Suggestions include currently
declared agents only and can be empty before the owning host reconciles a plain workspace.
Rust exposes `host_repositories`; TypeScript and Swift expose `hostRepositories`.
The same read is `st agents repos [--host HOST] [--json]`.

`terminal.create` takes a nonempty display `name` (up to 160 bytes), optional `host` and absolute
`cwd`. An omitted directory uses the selected daemon's directory. It declares a standalone PTY
at `pty/person/NAME/UUID` running `$SHELL -i` (fallback `/bin/sh`) with restart policy `never`.
Its returned ID is `terminal/pty/person/NAME/UUID`, usable with existing screen, attach and input
methods. `terminal.end` takes `target_id` and permanently stops this personal PTY, including before
it has started or after its shell exits. It requires no runtime incarnation or terminal sequence.

All three actions require the session's concrete person or trusted local agent, a snapshot fence and a stable idempotency
key. Agent creation needs `control.runtimes`; terminal creation and ending need `terminal.control`.
Their same-named action capabilities advertise availability. Terminal declaration admission checks
the creator in local writes and replicated claims. A local agent creates a shell at
`pty/agent/PATH/UUID`, owned by that exact seat; no action selects another actor. Existing fleet terminal read/control grants
continue to govern screen access and input.

`affected_ids` contains the agent ID or terminal ID. A completed action means the durable declaration
was applied; it does not promise that the harness or shell is ready. Follow the existing agent/runtime
views and attach after a running terminal appears. Exact action retries return the saved result.
Session-scoped declaration tags also recover a committed creation if its action receipt was lost,
without declaring a second member, even through another fleet gateway. Reusing a key for different
parameters fails. New first messages have a durable launch-attempt receipt; see the runtime contract
for its crash boundary.

Rust exposes `agent_create`, `terminal_create`, `terminal_end`; TypeScript and Swift expose
`agentCreate`, `terminalCreate`, `terminalEnd` with generated typed parameter bodies.

Messages tagged `dictated` carry a delivery-only line explaining that voice transcription may
contain mistakes. The stored text and body digest stay unchanged. Timeline message bodies carry
the message's optional `tags` array so clients can mark dictation without inspecting its text.

The agent resource exposes observed harness status as `harness_state`, `since` (RFC 3339),
and `observation: current | stale | missing`. `since` is the start of that state in that
runtime incarnation; repeated observations and changes to display details preserve it.
`observation` becomes stale after 90 seconds without fresh evidence, and is missing when
this runtime has no observation. Stale idle is exposed as indeterminate, never proven idle.
Desired runtime state and agent-declared work progress remain separate from harness evidence.

`GET /v1/client/status-history/{agent-id}` (`statusHistoryGet` in TypeScript) reads canonical
ordered transitions for one seat. Each item has `seat`, `runtime_incarnation`, `state`,
`observed_at`, and `reset`. A replacement runtime produces a reset immediately, even before
its first harness observation (`state: null`); no new state vocabulary is introduced.
The read retains at most 200 transitions/reset entries and only the last 7 days. It reports
`retained_from` and `complete: false` when either bound trims history or checkpoint tombstones
prevent proving completeness. Absence from incomplete history never proves continuity.
The paired gateway uses the same endpoint and `read.projections` scope. Current status
updates use the existing agents collection stream, including clock-driven staleness updates.

Same-state observations remain local except for a freshness publication at most once a minute.
That publication preserves `since` and does not add a history transition. Freshness uses the
source observation time, so replaying old evidence cannot make a stale seat current.
