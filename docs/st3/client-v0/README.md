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

### Mission run timing

`GET /v1/client/missions/{id}` requires `read.projections`, like other projection reads.
Its typed `run_details[].steps[]` includes `loop_round`, `loop_max_rounds`, `loop_reason`,
`next_wake_at`, `wake_reason`, `wake`, and `claim_expires_at`. A mission detail already carries
every run's steps, so these observations extend that contract rather than introduce a second
run-detail endpoint. The detail uses the same enriched `Store::mission_run` read as the trusted
`GET /v1/mission-runs/{run}` only for open runs and the latest finished run. Older finished
runs keep lightweight effective-step summaries. Headers and enrichment share one reader
snapshot; mission lists retain their lightweight summary reads.

`loop_round` and `loop_max_rounds` belong to the loop attached to that exact step in the current
generation; they are null for a non-loop step. `loop_reason` is the observed loop-state reason.
`next_wake_at` is the observed not-before time (earliest work eligibility), not a delivery promise
or a computed retry estimate. `wake_reason` is the deferral's blocked reason, otherwise the
observed wake failure. `wake` carries the assignee and its current harness state/incarnation,
attempt count, last attempt time, acknowledgement basis (`claim`, `consumed`, `delivery`, or
`turn`), and failure, even when no next wake time is known. It is null for finished runs
and finished steps. `claim_expires_at` is the effective claim lease expiry. All timestamps here are
RFC 3339 UTC strings; unknown values are null. Older servers may omit these optional fields.
Loop and wake enrichment is detail-only and bounded to open runs plus the latest finish;
null timing fields on lists or older finished runs are not proof of no loop or wake.
Clients can render `round N/M · wakes in …` without reading claim envelopes.

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
order: each has its `name`, ordinary resource `subject`, authored `kind` (`uri` or `subject`),
URI `resolution`, and nullable `reason`. A mission's list comes from its current published
revision. The owner stores typed edges, while the shared smallclaims subject stores the immutable
URI. Its ID is `resource/uri/` followed by the lowercase hex SHA-256 of the exact URI's bytes;
different owners naming that URI resolve to the same subject.

The list is read-only; change it by reapplying the seat declaration or publishing a new mission
revision (see [declared resources](../kdl-lifecycle.md#declared-resources)). Removing an edge does
not delete its subject or other owners' references. A URI edge has `resolution: "resolved"` and
a string `uri` when its locator is available, or `resolution: "unresolved"` and no `uri` while
the URI declaration is unavailable locally. Explicit subject edges have `resolution: "not-applicable"`
and no `uri`: they need no URI locator, and this state does not assert subject existence.
A daemon older than the field omits it; clients read a missing list as empty.

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
measured or no finite forecast fits the safe-integer duration bound of 9,007,199,254,740,991
whole seconds, `2^53 - 1`). Unavailable forecasts are null or absent; they are never clamped.
The producer and API projection share `MAX_SAFE_DURATION_SECONDS`. A stalled peer keeps its
sync notice even when its forecast is unavailable. Clients show the notice above the page.
The page omits it once the host has caught up.

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

For canonical structured publication values, use
`GET /v1/client/publication-definition?subject=mission%2Fexample%2Fdaily` (also accepts
`agent/` and `schedule/` subjects). Rust `publication_definition(subject)`, Swift
`publicationDefinition(subject:)`, and TypeScript `publicationDefinition(subject)` return
`PublicationDefinition` in the same snapshot envelope. Its `declaration` is the compiled
`MissionSpec` for missions, or normalized `DesiredSubject` for seats and schedules, with
`revision` and immutable claim `token`. This read requires `read.projections` and
`read.declarations`; environment values and embedded declarations are retained. Missing
subjects return 404 and oversized definitions are rejected without truncation.
These are the same values used by the [owned-set declaration diff](../owned-sets.md#declaration-diffs-and-readback).
A definition is never truncated:
when its serialized value exceeds `max_response_bytes - 4096` (reserving room for the envelope),
the server returns `validation-failed` rather than an incomplete AST or KDL document.

### Usage over a period

The optional `agent_messages` field adds a **daily estimate**, also shown by `st usage` and
stui. It counts distinct agent-to-agent message subjects, excluding people, daemon sends and
explicit delivery-probe/test tags, probe/soak seats and the audit's named test-title prefixes.
Receipts and duplicate send claims add no messages. Bodies are never classified. Counts are
maintained in the writer transaction and backfilled once on upgrade. Full UTC days use daily
recipient counters; the first and last partial days use an indexed send-time range.

`AgentMessageEstimate.days` contains at most the latest 31 UTC calendar days intersecting the
period, including zero-message days. Each row names `day_start_ms`, the clipped `since_ms` and
`until_ms`, `messages`, `calibrated_messages`, `low_microusd`, `high_microusd`,
`usage_cost_microusd`, `unpriced_tokens` and nullable `low_percent`/`high_percent`.
Usage costs use the same cumulative snapshots and baseline/reset rules as period rows, in the
same pass. A zero priced denominator makes the percentages unknown; unpriced coverage remains
visible. The range can exceed 100% and is never capped. This is a planning allowance that
includes useful work in message-associated turns, not measured overhead, waste or a confidence
interval. Subscription invoices and token shares are different measures.

The selected `doc/usage/agent-message-allowances` JSON document optionally supplies
`{ "source": "dated audit reference", "method": "calibration method", "recipients":
{ "agent/alder": { "low_microusd": 100000, "high_microusd": 200000 } } }`.
Seat names stay in the fleet's graph, rather than the public source. The response pins the
selected immutable document in `calibration`, and carries its `source` and `method`.
Missing recipient allowances use the $0.22–$0.33 API-equivalent fleet allowance, exposed as
`fallback_low_microusd`/`fallback_high_microusd`. A missing or invalid document uses the fleet
fallback for every recipient and reports no calibration. Recalibration changes future reads
of historical counts; the pinned document makes an exported report reproducible. Older daemons
omit the entire estimate, which clients treat as unavailable.

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
(a label such as `claude/<digest>`, or `DRIVER/unknown`), `driver`, optional `account_ref`,
`identified` (optional for older servers), and `plan`,
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
`identified` is true for a provider identity or declared account, false when both are missing;
it does not describe quota freshness. Identity-less history with no active reporting seat is
hidden once identified evidence exists for that driver. Active unknown evidence and every
identified account, including exhausted accounts with no seats, remain visible. Bound readings
use the same stable declared label as publishers, including older generic provider labels.
Partial reports without a weekly percentage do not replace or refresh a prior weekly source.
The durable reading survives member restarts. Consumers must check its original measurement time
and reset window; missing or stale evidence is unknown, never zero. `st doctor` reports missing,
stale, future-dated or already-reset weekly evidence for active accounts as `account-limits`.
See [account limits](../accounts.md#at-the-limit) for the policy and external backstop behavior.
Rows also carry optional `native_session_id` (the provider's session UUID, fenced by incarnation)
and `pricing_provenance`. The legacy `pricing` label remains readable. Each provenance entry
names `cost_source` (`provider_reported`, `computed`, or `unpriced`), disjoint token buckets,
`cost_microusd`, `reported_cost_microusd` and `unpriced_tokens`. Computed entries name
`price_table_id` (`st.api-list`), `price_table_version` (date plus SHA-256 of the complete table),
and `rates_usd_per_million_tokens` (`input`, `output`, `cache_read`, `cache_write_5m`,
`cache_write_1h`), with long-context multipliers already applied. Provider-reported entries
omit undisclosed table identity and rates; unpriced entries name the attempted table and omit
rates. Entries on rollups are cumulative; period reads subtract the matching baseline entry.
Different versions, sources and effective rates remain separate within the same rollup slot.
Historical claims without these fields remain readable; absent provenance or session binding
is unknown. After an upgrade, provenance covers only responses priced by the upgraded writer;
its bucket sums can therefore be smaller than the row's totals. Per-response rounding means
recomputing a contribution from aggregate token counts can differ slightly from the recorded cost.

The external per-request ledger remains canonical (#1419). These additions keep the existing
five-minute/stop publication cadence and latest/hourly/baseline retention bounds. They do not
change checkpoint selection or carriers, so they require no additional rules-version bump
beyond version 10 introduced separately by #1322 for native credential evidence. Typed imports and a
retention decision for native per-request records remain later work.

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

A card for an ask a mission step made carries that mission (`mission_id`), its run
(`mission_run_id`) and `blocked`: the step run that asked and waits for the answer, its name
(`step`), what it is for (`goal`) and its `attempt`. A standalone ask (`--new-run`) and an update
have no `blocked`. Show the step and its goal with the question, so the person can see what
their answer lets go on, and link the mission.

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
the result's text content is retained. Native result `details` objects are merged verbatim into
the normalized `tool_output` block's optional open `metadata`, preserving `wallTimeMs`,
`timeoutSeconds`, zero/fractional values, original units, and future fields. There is no
`tool_result` body metadata field; the complete native record remains unchanged in its
`source_record` block. Legacy tool-result blocks inside message content remain supported and
use the same block metadata shape.

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
- `idempotency_key`: unique within the paired session; never reuse it for another request;
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

New completed local response receipts last at least **7 days**. Clients must never reuse a
key for another request. After reclamation, a late retry cannot execute the original effect
twice: the daemon seeks its primary effect claim through the existing operation index,
including existing checkpoint operation tombstones. Document writes recover the original
binding response, even if a retry supplies different bytes and a current binding token. Other
associated writes return HTTP 409 `idempotency-key-expired`, with the key and original claim id,
when the saved response cannot be rebuilt. This is terminal (`retryable: false`), not a silent
success; inspect the committed outcome before deliberately starting a new action with a fresh key.

Seven days covers the phone's eight retries with backoff and normal retries within seconds to
minutes, including a daemon restart. Exact saved-response replay is supported for seven days;
durable recognition continues afterward. Receipts already present at the schema-17 upgrade
are grandfathered: their older claims do not contain the caller-key association. This PR never
expires those rows or rewrites those claims. A separate follow-up deletes the fixed legacy
cohort **30 days after actual deployment**, outside realistic client retry horizons. Operations
must record the deployment timestamp and due date in
[the legacy cleanup follow-up](https://github.com/compoundingtech/smalltalk/issues/1741).
That future deletion is an explicit legacy exception, not an indefinite dedupe guarantee.

Cleanup adds, drops and changes **no indexes**. It seeks the first rowid above a persisted legacy
watermark and examines at most 64 response rows in completion order. It stops at the first young
row; nonmonotonic completion deadlines can delay later expiry. Active-run receipts move to the
tail with a renewed deadline using UPDATE, preserving their companion rows. A future writer
that has neither a durable association nor an explicitly safe retry classification fails closed
and retains its response. The commit callback only signals a coalesced notification; pooled
reader deadline checks and the bounded writer transactions run off the request path, pausing
20 ms between chunks. UTC cleanup is capped by the newest committed claim time plus five
minutes through the existing accepted-time index. No claim clock means cleanup defers; a bad
RTC alone cannot reclaim recent receipts. Retries do not refresh expiry.
The clock anchor includes replicated claims, so a fast-clock peer can advance it. A pre-NTP boot
clock followed by a forward correction can shorten saved-response retention; durable operation
recognition still prevents a second effect, but byte-identical response replay can end early.
The expiry trigger's `unixepoch('subsec')` requires SQLite 3.42+ (bundled SQLite is 3.46).
Busy/locked failures retry with 200 ms to 5 s backoff; permanent errors stop visibly.
These response rows and their legacy watermark are local SQLite state, not replicated claims
or checkpoint authority. Existing canonical claims and checkpoint rules are unchanged.

| Local-response operation | Retry after the response expires |
| --- | --- |
| Document binding | Recover the original binding from its `doc.bound` operation claim; refuse terminally if only the checkpoint tombstone remains. |
| Mission/free-work start, mission output | Recognise the primary creation/output claim and refuse terminally if its response cannot be rebuilt. Deterministic run identity also prevents duplicate creation. |
| Revision proposal create/adopt/cancel, effectful approve | Recognise the primary proposal/generation claim; terminal refusal prevents repeating its effect. Already-approved no-op responses may be checked again. |
| Failed-step retry, run outcome, mission retirement | Recognise the primary effect claim and refuse terminally before another attempt, outcome or publication. Effect and response commit atomically. |
| Work claim/progress/complete/fail/release/extend/handoff, published renewal | Recognise the actual work claim and refuse terminally before another transition. |
| Quiet work renewal | No new claim: retry under current generation, attempt, actor/incarnation and lease checks. Extending a still-held lease is naturally idempotent; a released/expired lease cannot be renewed. |
| Declarative/owned-set apply, quick agent creation | Direct declarations carry a primary caller association. Compound publication operations retain their existing durable operation ids; identical retries are no-ops. Claimless unchanged apply is safe to recompute. A compound-only apply with different bytes and a reused key after expiry is a new apply, so clients must never reuse keys. Declaration and quick-agent response commit together. |
| Resource observation completion | Recognise the primary actual observation/state/delivery claim and refuse terminally before another delivery. A completion with no recorded change has no receipt/effect to repeat. |

Asks, attention actions, message sends and held effects are not pinned by this local-response
rule: their existing deterministic durable claim identities and action receipts provide dedupe.
Client actions keep their durable `custom.client.action-result` receipt; normal claim append
uses its existing operation identity and checkpoint tombstones. Cleanup never deletes these
claims. Rust SDK/CLI and stui use the client's transient-error classification; unknown codes
follow `retryable`, and this new code is terminal. TypeScript/phone `isTransient` and
`retryTransient` likewise stop on this code; the phone's fresh-fence `notApplied` retry list
contains only stale-fence/rate-limited. Swift preserves unknown codes and `retryable: false`.

Caller keys have one namespace across actors and nodes that share these claims. Use a fresh,
globally unique key for each new request; keys are not an authorization boundary. Reusing
another caller's key can return a terminal refusal identifying their original claim. Actor-scoped
keys would require a compatible namespace migration and are a separate follow-up.
Revision approval currently commits its approval before cutover and response storage; a crash
or concurrent retry in that interval can receive the same terminal refusal before seven days.
Inspect the proposal's current state rather than retrying its effect with a fresh key.
Compound publication receipts use existing operation identities. Checkpoint rules version 11
has no drop rule for `publication.operation`, so those claims remain available to rebuild the
operation registry; this release does not change that rule.

Claim event positions keep their local store index through schema-17 migration. Converting an
existing event table seeds its resume floor once at the current store frontier: older checkpoints
did not record enough position information to prove earlier cursors continuous. Lagging followers
receive one explicit HTTP 410 and must resync; a cursor equal to the upgrade frontier continues.
Fresh stores keep floor zero, and ordinary excluded-claim gaps never raise it. Event payloads are
read from claims; a local membership index records which accepted claims were eligible for the
feed. The temporary legacy payload table drains in transactions of at most 64 rows. Schema 17
requires the matching daemon on that node; an older binary refuses the upgraded database. Older
peers keep their own local event cursors and continue replicating the same claim vocabulary.
Schema 17 is a one-way database upgrade: take a supported backup before upgrade; downgrade
requires restoring that backup. The legacy migration window ends when the 64-row worker drains
`local_event_payloads`; its duration depends on existing rows and writer competition. No fixed
duration or throughput is promised before copied-store measurements.

Live claim bodies and positions do not change in place: local admission inserts them and replication
uses `INSERT OR IGNORE`, with checkpoint tombstones refusing dropped claims. Live checkpoint
deletion removes event membership and raises its floor before deleting the claim, in the same
transaction. Checkpoint proof renumbers positions only on its proof copy, after clearing local
projections, and rebuilds membership before comparing reader answers. Repair/replay also clears
and rebuilds membership; it does not update a live claim body behind an existing event pointer.
Projection tail reads bound membership before joining bodies, including during legacy migration.

`GET /v1/events/page` scans at most 201 eligible rows for a page limit of 1 through 200. Its
`items`, `next_after`, `has_more`, and `frontier` fields separate visible results from scan
progress. An owner filter can return no items while `next_after` still advances. Subject and owner filtering
always happen after the bounded global scan, including after migration; no subject index is added. With `wait=true`, an empty
page that advances the cursor waits 250 ms before returning, capped by the remaining requested
timeout. This paces following clients during migration without discarding eligible events or
scanning additional pages inside the request. Matching pages return promptly; `wait=false`
remains an explicit bounded catch-up read. Following clients, including the CLI, use `wait=true`.
The server deadline envelope adds the clamped requested wait (10 ms through 30 seconds) to
its ordinary 15-second read budget only when paged wait mode is enabled. Each bounded store
query retains the ordinary query budget; waiting and pacing do not extend its SQL work budget.
Trace follow
uses this continuation and registers its wake before reading the frontier. The legacy
`GET /v1/events` keeps its array shape, returns at most 200 rows, and reports continuation in
`X-ST-Next-After`, `X-ST-Has-More`, and `X-ST-Frontier`; owner filters and subject filters
during migration require the page endpoint. CLI condition, restart, and harness waits use pages.
A new CLI probes that route once. On a bare missing-route 404, it negotiates only a legacy daemon
that explicitly advertises `features.bounded_legacy_events=1`; otherwise it stops with daemon
upgrade guidance before requesting legacy history. Typed refusals, errors and HTTP 410 never
activate negotiation.
A cursor before the recorded checkpoint deletion floor receives HTTP 410 `cursor-gap` with
`full_resync=true`, `resume_floor`, and `frontier`. Refresh projections and establish a new
cursor explicitly; do not silently reset or keep retrying the old cursor. Client-v0 event feeds
continue returning their existing `oldest_cursor` and `newest_cursor` resync details. A bounded
replacement without an old cursor returns a continuation at least at the current frontier, so the
next request cannot loop on the upgrade floor. Raw event consumers exit with status 6 on
`cursor-gap` and print the floor/frontier. Trace followers also print an explicit
`st trace show ... --after-index FRONTIER --follow` continuation command on the same endpoint.
Inspect retained claims before opting into that continuation: the reported gap is never replayed.
Without `--after-index`, trace displays its bounded history then starts following at the observed
global frontier, even if that subject has no claims or its newest claim predates the floor.
Capabilities read their event cursor and floor together and advertise that observed frontier.
Replacing a
projection does not pretend to replay missed durable transitions.

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
| Terminals | `terminal.input`, `terminal.resize`, `terminal.attach`, `terminal.detach` | runtime incarnation; resize and a line of input (`mode` `line`) also require the screen sequence; raw and key input do not |
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

`agent.suspend` takes `{agent, reason?}` and requires the running incarnation and selected desired
revision in its fence. `agent.resume` takes `{agent, host?}` and requires the selected desired
revision. An absent `host` preserves single-host behavior. A different host requests portable
continuation under the same seat identity, with `suspension.phase` progressing through
`fencing-source`, `transferring`, `restoring`, `verifying`, and `resumed`. A failure returns to
`suspended` with a typed code and reason. `suspension.source_host` and `suspension.host` are optional
additive fields. See [suspending a seat](../suspend.md) for workspace and harness restrictions.

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

Claim-backed timelines project at most the newest 4,096 operations. A store query's continuation
means older rows still exist, not that retention deleted them. A non-retryable
`timeline-query-limited` system error entry explains this projection bound, including entries
whose retained append precedes the window and whose updates therefore cannot be projected
coherently. Page cursors reach only the materialized window, not those omitted store rows.
The notice remains in the newest page and does not invent an omitted sequence interval.
Availability notices are anchored at the latest materialized entry timestamp, with sequences
after the materialized window. A replicated snapshot's older clock cannot place them at the
oldest scroll-back boundary when local observations have advanced independently.
Conversation changes carry these projection notices when managed history changes, including
the first operation that crosses the bound. An updated entry outside the materialized window
causes the ordinary cursor-gap/newest-page refresh instead of an incomplete revision delta.
Changes to typed truncation entries also refresh the authoritative newest page, clearing an
obsolete prefix-unavailable notice when an interval establishes complete prefix coverage.
These session-stable projection notices are not evidence of overlap between a refreshed newest
window and retained older pages. Clients preserve paged history only when real transcript
entries connect those windows; a notice shared by otherwise disconnected windows does not.
Projection notices also do not determine the oldest real entry of an incoming window.
When preserving a genuinely overlapping older prefix, discard held projection notices:
the authoritative replacement frame alone supplies their current presence.

When the physical retained prefix starts after sequence one without a covering typed truncation
interval, a non-retryable `timeline-history-incomplete` system error entry explains that earlier
history is unavailable while complete retained entries remain readable. This notice does not
claim what the missing prefix contained. An update whose append is genuinely absent still
reports HTTP `timeline-history-incomplete` with `retryable: false`, `full_resync: false` and
`retained_history_incomplete: true`; a query bound does not excuse it. Retrying a fresh snapshot
cannot restore missing claims. Ordinary expired stream cursors remain retryable.
Prefix coverage considers every retained typed truncation append for the same owner and
incarnation at the snapshot, including intervals outside the 4,096-operation projection window.
The coverage query reads interval bounds only; it does not materialize older transcript entries.

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
and explicit revision fences still apply. Attach/detach do not require a screen sequence fence, and
neither does `terminal.input` in `raw` or `key` mode: keys a person types cannot be made unsafe by a
screen they did not see, and a program that redraws itself (a spinner) moves the sequence between
any read and any send, so a client that must match it can never type. A client may send such input
with no `terminal_sequence`; one that still sends it is not checked against it. A daemon older than
this refuses such input with "terminal control requires a sequence fence", and the client then reads
the screen and sends its sequence as before.
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

### Launch size

A client that draws seats in a pane of fixed size publishes it as a claim on the person, so a seat
st launches later starts at that size and the first attach needs no resize:
`POST /v1/claims` with `{"subject":"person/NAME","kind":"terminal.launch-geometry",
"actor":"person/NAME","fields":{"rows":ROWS,"columns":COLUMNS}}`. Only the person may write it,
and `rows` and `columns` must each fit a positive u16. At every terminal launch the reconciler
reads the newest such claim for its configured `person` and starts `pty run` with
`--rows ROWS --cols COLUMNS`; without a configured person or a claim, pty picks its own size. The
claim does not wake the reconciler, and a running seat keeps its size: resize it with
`terminal.resize`.

### Raw PTY transport

`POST /v1/client/terminals/{id}/raw-attachments` accepts
`{"runtime_incarnation":"PID:CREATED_AT","mode":"attach"}` (or `"peek"`).
The ordinary client response envelope carries `terminal_id`, `runtime_incarnation`,
`owner_host_id`, `mode`, and `stream_capability` in `value`. A capability expires after 60 seconds
and can open exactly one transport. It is bound to the gateway's authenticated session and person,
terminal, owner host, runtime ID and incarnation, and access mode. Projected-screen capabilities
cannot open raw streams, and raw capabilities cannot open projected streams.

PEEK capabilities live only in daemon memory: expiry, single-use consumption and session/person/
terminal/runtime binding are retained, but restarting the gateway invalidates outstanding PEEK
capabilities. Neither issuing nor consuming PEEK creates store claims. ATTACH keeps its durable
attachment and consumption claims.
The daemon retains at most 4096 unconsumed PEEK capabilities, pruning expired entries on issuance
and consumption. Full admission returns HTTP 429 `rate-limited` rather than evicting live tokens;
consume an outstanding token or wait for expiry before requesting another.

Open `/v1/client/terminals/{id}/raw-stream?incarnation=...&mode=attach` using WebSocket subprotocol
`st3.client.pty.v0` and secondary `st3.cap.CAPABILITY`. The credential and capability never appear
in the URL. Authentication, mode checking, single-use consumption, owner graph fencing and the
owner's kernel/registry incarnation proof all precede upgrade. Both modes require a concrete
person and `terminal.read`; `attach` also requires `terminal.control`.

Authenticated native clients can instead open `mode=peek` with only `st3.client.pty.v0`,
without a preceding POST or capability. Authentication, the concrete person and `terminal.read`
scope, live seat/incarnation fencing and revocable lease registration are still checked for every
open. Browser clients may keep the capability flow above. Who opened which PEEK is recorded in
observability logs/spans, not the durable claim graph.

Binary WebSocket messages are consecutive bytes of the original PTY protocol, not JSON screens.
Message boundaries have no PTY meaning. The client sends ATTACH (or PEEK) itself, and receives the
PTY's atomic SCREEN replay followed by live DATA, GEOMETRY and EXIT unchanged. The owner holds
one PTY connection for the transport lifetime. ATTACH and RESIZE therefore participate in normal
per-axis min-wins geometry with other persistent writers; PEEK cannot send input, resize, upgrade
to ATTACH, or contribute geometry. DETACH and closing the transport release the connection.
After ATTACH, an empty PTY frame of type 11 (`ResetInputModes`) can recover the daemon's
input modes without writing reset bytes to the child. PEEK connections and nonempty reset
frames are refused. The normal screen/history survive, and the daemon broadcasts the reset
as DATA. Older PTY daemons ignore this extension.
Raw clients cannot issue PTY lifecycle/CAS or ancestry-management commands through this capability.
Bounded chunks and socket backpressure preserve every byte; slow consumers do not skip output.

`st3-client::Client::raw_terminal_attachment` obtains the capability and
`raw_terminal_stream` returns a Tokio `UnixStream`. A terminal renderer can run its own PTY
parser, mode-aware paste, and geometry negotiation over it without learning about the fleet.
Each reconnect obtains a fresh capability for the same explicitly chosen incarnation; the
transport does not silently reselect a replacement runtime or replay input.

Rust `Client::raw_terminal_peek(terminal_id, runtime_incarnation)` opens this direct PEEK path and
returns `RawTerminalStream` with `.stream` and `.activity`. The caller sends its own PTY PEEK frame;
foreground use is explicitly renewed with `.activity.selected_use()`. There is no automatic
renewal: the existing 60-second idle and 300-second absolute deadlines, issuer/owner revocation and
fail-closed watch rules apply. Reconnect using the same chosen incarnation; no capability is needed.

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

Revoking a pairing from a non-issuer returns HTTP 409 with `code: issuer-required`,
`retryable: false`, and `details.issuer_host_id` naming the authoritative issuer.
The caller must direct revocation to that issuer rather than retrying the same gateway.

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
The negotiated client response ceiling is 1 MiB.

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

## Person arrangements

An arrangement is shared sidebar organization, not a private pane workspace. Discover
`arrangements` capability version 2 and the `read.arrangements` / `control.arrangements`
scopes. Glass identity, pane bodies, and privacy are unchanged. Authenticated persons and
trusted fleet agents read and write in today's free mode as their **real actor**; an agent
never impersonates the owner. Ownership is permanently the person in
`arrangement/person/NAME/lowercase-UUIDv7`, independent of run or session lifetime.
Restriction belongs to the upstream principals/grants system, not an arrangement-specific
opt-in or ACL. Anonymous access is refused. A paired person selects only their own
collection; a trusted local fleet agent explicitly selects a fleet person.

Generic status views omit arrangements, like glasses; current arrangement reads use the dedicated client routes, and later arrangement edits do not invalidate historical status frontiers.

Read `GET /v1/client/arrangements?person=person%2FNAME` (ordinary cursor/limit pagination)
or `GET /v1/client/arrangements/{person_name}/{uuid}`. The owner selector is mandatory:
an agent identity is not an implicit collection owner. Lists contain typed `Arrangement`
resources in an ordinary page. Live details contain `id`, `kind: "arrangement"`, `owner`,
`revision`, `updated_at`, `deleted: false`, and this versioned body:

```ts
type Register<T> = { value: T; revision: string }; // winning ClaimId
type ArrangementBody = {
  version: 1;
  name: Register<string>;
  folders: Record<string, {
    name: Register<string>;
    position: Register<{ parent: string | null; key: string }>;
    tombstone: Register<true> | null;
  }>;
  placements: Record<string, Register<{ folder: string | null; key: string }>>;
};
```

Existing phone pairings lack `read.arrangements` and must be re-paired to read arrangements.

Folder IDs are stable lowercase UUIDv7. Placement keys are graph subjects, never PTY or
session IDs. An optional `resolved: {parents, folders}` supplies effective folder parents
and placement folders; null means root/unfiled. It does not rewrite raw registers.
Names are nonblank, contain no control characters, and are bounded to 256 UTF-8 bytes; canonical base-62 fractional-indexing
keys are bounded to 128 bytes (for example `a0`, `a1`, `Zz`). Integer-part lengths must
be canonical and fractional suffixes cannot end in zero. Each edit has 1–1,024 operations
and its compact UTF-8 JSON operations array is at most 1 MiB, locally and on replication.
Local full projected resources (including headers, register revisions and resolved
locations) are bounded to 512 KiB (`max_arrangement_resource_bytes`), 1,024 folders
(including tombstones), and 4,096 placements. Local creation admits at most 100 live
arrangements per person. These cumulative quotas apply only to local admission, not to
a remotely replicated concurrent union. Heads retain that union without arrival-dependent
dropping or canonical-cap truncation. Retirement bypasses cumulative overflow so an
oversized remote union can still be retired. Per-operation shape, name, key, ID and claim
bounds still apply on replication. Clients must not treat a lexical JSON Schema key
pattern as the full fractional-key validator.

Submit `POST /v1/client/actions` with `type: "arrangement.edit"` and typed parameters
`{subject, owner, operations}`. `owner` is required on every edit and must equal the
immutable person in the subject. Operations are tagged by `op`:

| `op` | Required fields besides `op` |
| --- | --- |
| `create` | `name` |
| `rename` | `name` |
| `folder.create` | `id`, `name`, `parent: null \| folder ID`, `key` |
| `folder.rename` | `id`, `name` |
| `folder.move` | `id`, `parent: null \| folder ID`, `key` |
| `folder.delete` | `id` |
| `subject.place` | `subject`, `folder: null \| folder ID`, `key` |
| `membership.migrate` | none (existing identity, sole operation) |
| `retire` | none |

Each atomic edit may touch a register at most once; retirement or migration must be its only operation.

Creation declares the name and may atomically include folders and placements. Null
placement folders unfile subjects; changing the key reorders them. Include any necessary
rekeys in the same atomic edit. Only touched registers change: stale layout revisions
merge rather than replace unrelated fields. For layout edits, the target arrangement's
entry in `fence.subject_revisions` is a layout base, not a compare-and-swap precondition.
For `retire`, that entry instead requires the current arrangement revision to match in
the writer transaction: a mismatch returns `stale-fence` and appends nothing. An unfenced
retire is unconditional. To fold a duplicate losslessly, read it, fold its contents into
the winner, then retire it fenced on the revision read; on refusal, reread and refold.
The existing action ID, snapshot fence, and session-scoped idempotency key remain required;
stale graph identity/authority and launch revision fences for other subjects refuse the edit
atomically in the writer transaction. Attention episodes and external native-session revisions
retain the existing projected preflight checks; external filesystem state is not governed by
SQLite admission and cannot be made transactionally atomic with graph writes.
Retry of the same action identity, parameters and key returns the saved receipt, even
after later edits or retirement. Fences are excluded from request identity: they may
be refreshed after refusal, and accepted retries replay before fence validation.
Changing other input with the same key returns `idempotency-conflict`. A completed
result's `affected_ids` names only the arrangement and `arrangement_revision` identifies
the accepted claim, not a promise that its registers will remain winners.
A second `create` for an existing arrangement under a different idempotency key returns
typed `arrangement-exists` with `retryable: false`; clients can distinguish a create
race from an internal failure. Exact retries of the original create return its receipt.
Folder creation reusing a live or tombstoned ID returns `arrangement-folder-exists`.
Generated clients preserve the following admission/validation codes as typed,
non-retryable refusals rather than `internal`; callers do not parse error wording:

| Category | Codes |
| --- | --- |
| Create race | `arrangement-exists`, `arrangement-folder-exists` |
| Lifecycle/structure | `arrangement-retired`, `arrangement-folder-deleted`, `arrangement-cycle` |
| Bounds/authority | `arrangement-limit`, `arrangement-body-too-large`, `arrangement-owner-forbidden` |
| Validation | `invalid-arrangement-subject`, `invalid-arrangement-action`, `invalid-arrangement-operations`, `invalid-arrangement-folder`, `invalid-arrangement-name`, `invalid-arrangement-key`, `invalid-subject-reference` |

The ordinary `not-found`, `forbidden`, `stale-fence` and `idempotency-conflict` codes
retain their existing meanings.

`arrangement.edited` claims are durable. Each register uses canonical maximum
`(accepted_at_unix_ms, batch origin, replica_sequence, batch_id, record position, claim_id)`,
including the canonical legacy position fallback, rather than client HLC or arrival
order. Offline writes therefore order by admission. Rename and move survive each other.
Folder tombstones are remove-wins and final for that folder ID; they retain position
and never delete subjects. Children and placements resolve through deleted folders to
the nearest live ancestor, or root; unknown folders resolve root/unfiled. Missing roster
subjects retain their placement for their return. Local self/descendant moves are refused.
Concurrent cycles cut the greatest canonical position-register winner (folder ID
tie-break) to root. Sort siblings by `(key, folder_id)` and members by `(key, subject)`.
Retirement is permanent for an arrangement ID; it disappears from live reads and streams.
Ending a reference never retires an arrangement or deletes a referenced subject.

Admission transactions materialize register heads, tombstones and owner-indexed current
rows. List/detail and write validation read these heads, never edit history. Replay may
rebuild them once; projection digests and checkpoint proofs include the same read answers.
There is **no new checkpoint drop rule**: agent-authored edits are durable too.

Subscribe on `st3.client.collections.v0` with
`{kind: "subscribe", id, collection: "arrangements", person: "person/NAME", limit: 100}`.
Person is required and grants are checked on every read. Bounded authoritative snapshots,
full resource upserts, removed IDs, complete order, and reconnect snapshots follow the
ordinary collection contract.
An optional `subject: ArrangementId` selects only that arrangement (zero or one items,
`has_more: false`), so a selected Sidebar cannot fall outside a busy owner's byte/count
window. Its owner must equal `person`; mismatches are refused. Snapshots, upserts and
retirement removals retain the ordinary collection semantics. Omitting the filter
keeps owner-wide prefix windows unchanged.
Arrangement list and stream windows also fit a byte budget: the 1,048,576-byte response
ceiling reserves 128,000 bytes for the envelope. A byte-shortened window sets `has_more`
and retains full resources, not truncated registers or placements. A replicated full
resource above the remaining 920,576-byte budget returns explicit `validation-failed`
rather than silently omitting part of its layout. Detail reads enforce the same single
resource bound. Reconnect takes a new authoritative bounded snapshot; paired-session
revocation, grant changes and expiry are checked again on each read.

Generated Rust exposes `arrangements_list`, `arrangements_get`, `arrangement_edit`, and
`CollectionStream::subscribe_arrangements`; TypeScript exposes `arrangementsList`,
`arrangementsGet`, `arrangementEdit`, and `CollectionStream.subscribeArrangements`;
Swift exposes `arrangementsList`, `arrangementsGet`, `arrangementEdit`, and
`arrangementsStream`. Detail methods take the person name and UUID separately.
All three arrangements stream helpers accept an optional `subject`; Rust takes
`subject: Option<&str>` after `limit`, TypeScript takes `subject?: ArrangementId`
after `limit`, and Swift takes `subject: String? = nil`.
Rust operations reuse `st3_schema::arrangements::Operation`; TypeScript and Swift have
typed operation unions, not arbitrary JSON bodies. Regenerate all clients with
`cargo run -p st3-client-codegen`; verify freshness with
`cargo run -p st3-client-codegen -- --check`.

Legacy custom-to-v1 Fractal migration is client-owned: discover capability, pause legacy writers, fold old
`custom.fractal.sidebar` HLC history once, and durably stage the snapshot, target ID,
exact typed operations and idempotency key. Preserve stable IDs, import order (keys are
re-derived), tombstones and placements. Fractal sorts legacy siblings by `(key, ID)` and
deterministically derives canonical fractional keys for each folder and placement list.
Retry that exact staged request until acknowledged and readable, then switch
exclusively to arrangements. Old stamps remain provenance, not live ordering. Keep staged
state on failure, leave immutable custom history, and never dual-write. There is no
upstream Fractal-specific importer.

### Ordered memberships and new version-2 layouts

New arrangements opt into a folder-only body with `arrangement.edit` parameters
`{subject, owner, version: 2, operations: [{op: "create", name}]}`. Version is accepted only
on creation; omission creates v1. A v2 body retains the versioned name and folder registers
but has no `placements`. Unmigrated v1 bodies and `subject.place` remain supported.
If offline v1 and v2 creates collide on one unmigrated identity, retained unrepaired v1
creation or placement authority preserves v1; v2 pairs remain retained but inactive.

#### Migrating existing placement authority

Discover granted capability `arrangement-placement-migration` version 1. Select the
existing canonical arrangement identity before migrating (migration never creates or
reselects an arrangement). Submit `arrangement.edit` with
`{subject, owner, operations: [{op: "membership.migrate"}]}` and no `version` field.
Migration must be the sole operation. Observe the same arrangement resource with
`body.version: 2`; its owner, name, folder IDs, parents, keys and tombstones are unchanged.
Each historical placement becomes a pair retaining the original claim revision,
canonical winner and actor provenance, including retired and not-yet-known members.
Migration writes a retained authority marker, not fresh placement winners.

The marker and backfill commit in one writer transaction: an interrupted request leaves
v1 intact or v2 fully active. Retry the exact action ID/idempotency key for its receipt.
Independent migration reruns are harmless and do not overwrite subsequent pair edits.
After cutover all placement readers/streams use the membership API below. V1-only
strict readers receive an explicit v2 body without `placements`, not an empty v1 fallback.
Local `subject.place` actions are refused with `invalid-arrangement-operations`.
A racing local action either commits before cutover and is included, or is refused.
Late replicated/offline v1 placement claims remain decodable and are routed into the
same ordered pair LWW reducer using their original winners; they cannot revert to v1.
Retained historical placement registers are replay-boundary sources only, never
an active v2 read/write authority. Existing membership writes compete in that same reducer.

Rules version 15 and shared projection identity
`st3.shared-projections.arrangement-placement-migration.v1` distinguish migration-aware
peers/checkpoint witnesses. Upgrade participating daemons before migrating or sealing
new checkpoints; older daemons may quarantine the new operation. There is no new drop
rule, irreversible claim rewrite or automatic binary rollback. Keep a pre-upgrade backup.

#### Membership pages and actions

Read `GET /v1/client/arrangements/{person_name}/{uuid}/memberships?person=person%2FNAME`.
The response is an `OrderedMembershipPage`, with `collection: "ordered-memberships"` and
rows `{id, container, member, position: {bucket, key}, revision}`. `id` equals the member's
stable subject; root is an explicit null bucket. Indexed order is `(effective bucket, key,
member)`, with root first. Cursors bind the person, container, limit and snapshot; repeat those
filters and reread after `page-cursor-expired`. Retired or unknown members are hidden, not
deleted. Same-ID redeclaration restores the retained position, while runtime stopped or
suspended observations do not retire a declaration.

Submit `arrangement.membership.edit` with `{subject, owner, operations}`. Each operation is
either `{op: "place", member, bucket: null | FolderId, key}` or `{op: "remove", member}`.
The bucket field is mandatory even for root; bucket and key win atomically in canonical
claim order. Removal retains an absent winner. These are claim-backed pair records, not
new graph subjects. Real actors, owner checks, idempotency receipts and subject fences use
the ordinary action protocol; a supplied container fence must match in the writer transaction.
Local admission allows 4,096 live members, not 4,096 lifetime pairs, and never truncates a
replicated union. An edit has at most 1,024 unique operations and a 1 MiB serialized bound.

Subscribe with `{kind: "subscribe", id, collection: "ordered-memberships", person,
subject: ArrangementId, limit}`. This bounded held window emits typed membership snapshots
and changes, complete order and refill after removals/lifecycle changes. Membership rows are
not `Resource` values; the schema uses separate frame variants and Rust emits
`MembershipSnapshot`/`MembershipChanges` events. Generated clients expose typed membership
page, action and subscription helpers. V1 membership reads/actions refuse rather than
pretending legacy placements are an empty v2 collection. No new checkpoint rule drops
hidden pairs, absent winners or their lifecycle dependencies.

Every membership `snapshot` and `changes` frame also carries a required
`membership: {container, live_count, changed_index}` describing the whole container, not
only the held window. `live_count` counts live members. `changed_index` is an opaque,
host-local, monotonic invalidation frontier scoped by the frame's `snapshot.host_id`: when
it moves, the container may have changed, possibly outside the window, and a consumer that needs the
complete set rereads it. A `changes` frame may therefore repeat the window's rows, order and
`has_more` while reporting new state. `changed_index` is not a canonical revision, a count of
global history or a winner; it never compares across hosts and stays out of shared digests and
checkpoint answers. A frame without `membership`, or with any of its fields missing, is
refused. Resource collection frames carry no membership state.

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

A timeline message body whose sender is a person carries `provenance`: the `verdict` every member
recorded for the message's signature (`verified`, `unsigned`, `held` or `invalid`, with a `reason` for
the last two), and for a signed one the `signer`, the `key` and the `device` the key was granted to,
by the label it was given when it paired (`example phone (secure enclave)`). Clients show it beside the
sender; an `unsigned` message is usually just older than signing and shows nothing. An agent's
message carries none. `GET /v1/messages/read/{id}` (`st conversations read`) and `st subject show`
for a message give the same object.

Messages tagged `dictated` carry a delivery-only line explaining that voice transcription may
contain mistakes. The stored text and body digest stay unchanged. Timeline message bodies carry
the message's optional `tags` array so clients can mark dictation without inspecting its text.

Agent projections include optional `workspace` and `checkout {repository, base, branch}` from the declaration, so clients can label a new seat before it launches. These describe the requested checkout; `state` and `fault` report whether launch succeeded. Older daemons omit the metadata. Both new-agent forms consume `host.repositories`, permit an absolute path typed on the selected host, keep the base editable, and leave an empty repository as a plain workspace. Their wire parameters and worktree label share `fixtures/clients/agent-launch.json`.

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

## Exact terminal lookup

`GET /v1/client/terminals?owner=agent%2Fexample%2Fworker&state=running` matches the
exact `owner_id` and projected `state` before the page limit. Owner lookup reduces only
that subject at the selected snapshot rather than scanning the fleet. Both query fields are
optional and combine with AND; there is no prefix or short-name resolution. An unknown
owner returns an empty page. State filtering uses the projected state, including
`unreachable` when runtime authority is unavailable. `history=true` (CLI `--all`)
retains its existing meaning; filters do not add stopped history by themselves.

The CLI equivalent is `st terminals ls --owner agent/example/worker --state running --json`.
Rust exposes `terminals_list_filtered`, Swift `terminalsListFiltered`, and TypeScript
`terminalsListFiltered`. Existing list methods, ordering, default limit (50), maximum
limit (200), and unfiltered paging are unchanged. Filtered pages echo `owner` and `state`
in `value.filters`; other collections ignore these terminal-only fields without acknowledging
them or binding their cursors to them. Terminal cursors bind those values, so every
continuation must repeat them.
Changing or dropping a filter while continuing returns `page-cursor-expired`.

This is an additive client read contract change: the operations manifest documents the
optional query fields; resource schemas, API versions, capabilities and stored claims
are unchanged. Old clients still list new servers normally. New filtered client methods
refuse an older server that omits the requested filter acknowledgment, with an upgrade
message, instead of returning an unfiltered page or scanning pages on the client.
A running projection is replicated evidence, not a successful PTY handshake: callers
must still use the existing incarnation-fenced attachment routes to attach.

The real-daemon mixed-build regression is a **manual proof**, not a CI gate: run

```sh
ST3_TERMINALS_COMPAT_BIN=/path/to/older/st3 cargo test -p st3 --test integration fleet::terminal_owner_filters_are_safe_across_mixed_builds -- --ignored --exact
```

Choose a binary built before these terminal filters. The fake legacy-server refusal
and escaping test, server filter/paging tests, and TypeScript client checks run in CI.

### Owner-native conversation blocks

With `X-St3-Features: conversation-blocks.v1`, native timeline entries include
optional `body.blocks`. Top-level timeline types stay unchanged for old Swift/iOS
and stui decoders. Block kinds are open strings; an unrecognized native block keeps
its complete JSON in `payload.raw` before transport bounding. Reasoning explicitly
present in the harness transcript and full structured tool arguments are shown
without secret or token filtering. Without the feature, the server returns known
text/tool/status bodies with visible size-limit notices. Old clients can read those
fallbacks but do not fetch images or expand a chunked remainder.

`read.projections` authorizes **raw native conversation content**, including full
arguments, output, reasoning shown by the harness, unknown JSON and images. It is
the existing scope for pages, deltas and owner forwarding, and also governs
`GET /v1/client/conversations/{id}/content/{reference}/chunk?offset=N`. Terminal
write or message-attachment scopes are not required. A local read-only Unix client
already has this scope; a paired client needs it in its active delegated grant.

Image pixels stay off timeline pages. Blocks and nested native images carry opaque
owner references. Every fetch verifies the authorized session, entry/revision and
native source identity; no client-supplied file path or image URL is accepted. The
ref encrypts the owner-located source descriptor, so native chunks do not inventory
other sessions and the path is not exposed. Managed chunks also recheck the current
owner binding. A
chunk contains base64 `data`, `media_type`, `offset`, total `size` and nullable
`next_offset`. Chunks hold at most 256 KiB of decoded bytes; native image reads are
limited to 32 MiB, with explicit errors. Transcript URLs are never fetched by the owner. Oversized JSON
payloads show an 8 KiB UTF-8 prefix labelled as truncated JSON text, and a reference
recovers the full valid JSON. The existing 1 MB page bound still applies.

Edited records, replacement, managed binding changes and owner restarts invalidate references;
append-only growth preserves existing refs:
HTTP 410 `conversation-content-invalidated`, `retryable: true`, and
`details.full_resync: true` tell the client to reload before fetching again. A
reachable owner with unreadable/missing bytes returns `transcript-unavailable`;
unreachable owner reads retain `remote-unavailable` and carry
`details.availability: owner-unavailable`. The managed transcript notice similarly
labels `transcript-unavailable`; retiring its stored-history fallback is owned by
the separate no-agent-history mission. This contract adds no durable content class,
claim kind, retention rule, spool, or image blob copy.

Native conversation `read.projections` grants full transcript, chunk and image access,
including secrets in exposed reasoning, tool arguments/output and unknown JSON.
Local people and agents, anonymous local read-only Unix readers, and default paired
phones and wall displays have this access. No content is scrubbed. Refs die on a
daemon restart; clients must reload the timeline. Four expensive owner timeline/chunk
reads can run concurrently; busy reads return HTTP 429 `rate-limited`, including
managed timeline reads. Clients must back off and retry.
Known blocks can use `payload: {body_ref: true}` to refer to the containing fallback
body without duplicating its bytes. Content refs authenticate one native record;
chunk reads do not rebuild the session. The owner never requests transcript HTTP(S)
URLs. External images use an `image_link` block for explicit client opening; file
reads are restricted to content-addressed files in the bound Pi/OMP blob store. MIME comes from passive image
signatures, with SVG/HTML/unrecognized bytes returned only as opaque octets.
