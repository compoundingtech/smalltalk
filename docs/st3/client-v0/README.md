# st client v0 contract

Status: implemented client boundary with operational projections, resumable events, authenticated
pairing, fenced actions, streamed terminal screens, and generated Rust and Swift clients. The files
in [`schemas`](schemas) and [`fixtures`](fixtures) are the normative wire examples. Rust and Swift
clients consume the same JSON; no client parses CLI output, Markdown, KDL, claim envelopes, or
harness transcript files.

The reusable Rust package is [`crates/st3-client`](../../../crates/st3-client) and supports both the
local Unix socket and authenticated Fabric-loopback HTTP. The Swift package is
[`clients/swift/St3Client`](../../../clients/swift/St3Client). The Expo TypeScript client is
[`clients/typescript/st3-client`](../../../clients/typescript/st3-client). Regenerate all three
clients' contract tables with `cargo run -p st3-client-codegen`;
CI and local verification use `cargo run -p st3-client-codegen -- --check` for byte stability.

## Boundary and transport

The client API is a projection and command gateway, not a graph replica. Its version is
`st3.client.v0` and its routes live below `/v1/client`. A local client connects to the daemon's Unix
socket. A remote client connects to a loopback-only gateway through authenticated Fabric. The
daemon and gateway MUST NOT bind this API to a non-loopback TCP address.

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
single-use attachment capability. Begin pairing over the trusted local socket with, for example,
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
bound to the snapshot ID, filter, page size, and final sort tuple, and expires no earlier than the
advertised `cursor_expires_at`. A cursor cannot silently move to a newer snapshot.

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

`work.done` takes `target_id`, `episode`, nonempty `summary`, and optional string `evidence`.
Only the assigned person or a session explicitly delegated by that person completes it. The
requester may instead use `work.cancel-ask`. Completion resumes a live origin in the same attempt
with a new readiness epoch; the response and evidence stay on the source. CLI equivalents are
`st work ask --for PERSON --title TEXT --reason TEXT --step STEP --as AGENT --idempotency-key KEY`
(or `--new-run NAME`) and `st work done STEP --as PERSON --summary TEXT`.

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

`mission_authority` lists the missions the agent may publish, start, revise, and cancel, as exact mission
IDs or terminal `/*` namespaces. Its `source` is `declared` when the declaration carries
`mission-authority`, `default` for a person-declared top-level seat `fleet/PROJECT/...` (which
holds `fleet/PROJECT/*`), and `none` otherwise. It is `null` for an agent with no current
declaration. Cancellation requires an explicit `cancel` rule and is excluded from the default.
Trusted local agent sessions may invoke `mission.cancel` with the current generation fence;
the daemon checks the mission path against their current declaration. Other mission actions
on client-v0 retain their person requirement.

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
invalid request fields. Repeating the same key and byte-equivalent action returns the original
result. Reusing a key with different bytes returns `idempotency-conflict`. Stale generation,
revision, incarnation, or snapshot fences return `stale-fence` without a partial mutation.
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
| Terminals | `terminal.input`, `terminal.resize`, `terminal.attach`, `terminal.detach` | runtime incarnation and terminal sequence |
| Pairing | `pairing.begin`, `pairing.complete`, `pairing.revoke` | pairing/device revision where applicable |

`runtime.stop` publishes a stop for the selected member. `runtime.restart` terminates the current
incarnation of a member with an `always` restart policy; its desired state then starts the next
incarnation. `runtime.reset` publishes a restart-window reset for a run-owned member. A client
submits the runtime resource ID as `target_id` and copies its `incarnation_id` and
`desired_revision` into the action fence. Runtimes with no selected desired state have a null
`desired_revision` and cannot use these controls.

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

## Pairing and remote access

Pairing is device-to-person. Its begin request names the concrete initiating `person/*` identity and
is accepted only on the trusted local Unix API; the daemon persists that person as the delegator.
The response shows a short-lived single-use code and pairing ID. A remote device reaches the
loopback gateway through Fabric, proves the code, supplies its public key, and receives a scoped
credential bound to that key. The resulting session returns the exact delegated person, its derived
device-session actor, and granted scopes.
Pairing codes expire after five minutes and reveal no fleet secret. The remote device cannot
request its own actor or scopes. By default the trusted local begin grants projection reads,
terminal reads, attention control, and launch control. For an intentionally trusted device that
needs Chat sends, mission/work actions, runtime control, and terminal input, the initiating person
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

Terminal access is a client protocol, not raw PTY ownership. The server sends screens, never PTY
bytes: each screen is complete and replaces every earlier one, so nothing is replayed and a client
that falls behind skips to the latest screen. Interactive attach from a terminal on the owning host
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

`terminal.attach` returns a short-lived, single-use stream capability and URL bound to the
authenticated session, terminal, and runtime incarnation; `terminal.detach` idempotently invalidates
that viewer. A client opens the URL on the same Unix or Fabric-loopback gateway with WebSocket
subprotocol `st3.client.terminal.v0`. Authentication, single-use capability consumption, and
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
line per row, and `next_sequence`, the fence for input and resize. `revision` digests the rest of
the screen: equal revisions mean equal screens, and a stream never sends the same revision twice.

Each line keeps its plain `text`, without trailing spaces, and adds `runs`: styled text from column
zero. A run has `text` and, when they differ from the terminal default, `fg` and `bg` colors and
`bold`, `dim`, `italic`, `underline`, and `inverse` flags that are present only when set. A color is
a palette index from 0 to 255, where 0 to 15 are the client's ANSI theme colors, or a `#rrggbb`
string. The runs spell `text`, followed by any trailing blanks that are visible because of their
background, inverse, or underline. Hidden text is sent as spaces.

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
the next one, so a long poll's answer is never cut short on its way back. A read that no peer can
carry fails with `remote-unavailable`.

Read-only terminal scope permits screens but rejects input and resize. Screen payloads obey
negotiated byte limits: at most 200 lines and 4096 bytes of text per line, with explicit
`redacted` and `truncated` markers.

## Errors and evolution

Errors have `error_version: st3.client.error.v0`, a stable kebab-case code, safe message,
`retryable`, structured details, and optional `retry_after_ms`. Required v0 codes are `not-found`,
`forbidden`, `unsupported-capability`, `validation-failed`, `idempotency-conflict`, `stale-fence`,
`cursor-gap`, `page-cursor-expired`, `rate-limited`, `runtime-not-local`,
`runtime-authority-indeterminate`, `remote-unavailable`, and `internal`.

Adding optional fields is compatible. Removing or retyping a field, changing ordering or token
rules, adding a required action parameter, or changing action semantics requires a new capability
version or API version. Clients preserve unknown enum cases for display but never send an action
whose capability version they do not understand.

## Conformance assets

[`schemas/client-v0.schema.json`](schemas/client-v0.schema.json) contains the shared wire types.
[`schemas/operations.json`](schemas/operations.json) is the machine-readable route/action/capability
manifest. [`fixtures/manifest.json`](fixtures/manifest.json) maps every golden fixture to its root
schema definition. The Rust tests validate fixture coverage, IDs, ordering, fences, timeline links,
and deterministic preview tokens. Ignored baseline tests exercise the missing implementation and
are intentionally red until the corresponding server work lands.

## Private glasses

A glass is one person's named workspace. Its stable subject is `glass/person/NAME/UUID`;
clients generate a lowercase UUID (stui uses UUIDv7). Renaming changes `body.name`, never the
ID. Names are free text and need not be unique. The client handles name lookup.

`GET /v1/client/glasses` returns the ordinary paged resource list, ordered by ID, and
`GET /v1/client/glasses/{uuid}` returns one resource. The authenticated session determines
its person; these routes accept no owner selector. Anonymous sessions and agents have no glass
access. Paired devices need `read.glasses` for reads and `control.glasses` for writes. New
limited pairings include both grants. Existing devices with explicit grants need a new pairing
if they lack them. Discover the granted `glasses` capability (version 0) before migrating local
storage; it is granted when the session has both read and write access.

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

The structure is `{name, tabs:[{title?, layout}]}`. A layout is `{pane: "opaque key"}` or
`{split: "right" | "below", children: [layout, layout]}`. Pane keys convey no authority. No
focus, scroll, selection, ratios, or last-used glass is stored. Empty `tabs: []` is valid:
clients supply their implicit Home locally. Names and pane keys must be nonempty; splits have
exactly two children, and no unknown structure fields are accepted.
The daemon advertises limits: 65,536 bytes of compact UTF-8 JSON per body, 32 layout levels,
1,024 layout nodes across all tabs, and 100 live glasses per person. The response ceiling is
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
