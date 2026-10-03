# st fleet replication

Fleet replication is optional. A node outside a fleet is a complete local-only st system.
`st doctor` reports this default as `pass` with “no fleet is configured” and points to
`st fleet create` or `st fleet join`. It reports “intentionally local-only after leaving its
fleet” only when `st fleet leave` has recorded that explicit decision in `left-fleet.json`.

A laptop running only stui can instead be a [paired client device](client-only.md), with no daemon
or replica. Devices read and act through a member's client gateway and are not sync peers.

Replication makes the logical authority equal across configured nodes. It does not make the SQLite files byte-identical.

## Sync invariants

Every shared projection folds claims in the canonical total order: numeric acceptance time,
writer, replica sequence, batch identity and immutable position, with claim identity as the final
tie break. Arrival order belongs to local cursors, caches and operational overlays. Equal claims
must produce equal shared rows and selected source identities through incremental admission,
restart replay and checkpoint trimming.

The digest covers everything that is synced: authenticated claim and blob identity, and every
shared logical projection and its shared columns. Each shared table or claim-derived source has
its own digest so a mismatch names the source. Local receipt metadata, physical indexes, secrets,
lease overlays and live reachability are excluded explicitly. Retained history and checkpoint
tombstones represent the same logical source identity.

Uploaded bytes in `local_blobs` are staged locally until a durable claim references them.
Claim admission promotes those bytes into `blobs` in the claim's transaction; every column of
`blobs` remains in the shared digest. Unreferenced uploads never enter envelopes and cannot be
compared as shared authority. On upgrade, retained claim references, document bindings and valid
received blob records identify existing shared bytes. Other bytes move to local staging without
being deleted. If checkpoint tombstones already removed historical references, the upgrade
conservatively keeps all existing shared blobs.

A keyed worker signs batches beyond its last processed batch, including envelopes another
unkeyed process already sealed at startup. Signature requests recover missing signatures after
an upgrade. Equal envelope inventories alone cannot prove equal claim admission.

`store::tests::canonical_audit::every_shared_projection_agrees_after_shuffle_restart_and_checkpoint`
checks both invariants by comparing shared rows, selected readers and per-table digest oracles
across isolated stores. `shared_folds_never_order_by_local_arrival` rejects raw shared arrival
folds, and `every_persistent_table_has_a_projection_scope` rejects unclassified tables. A new
shared table must join the shuffle test's inventory and history fixture,
the canonical ordering guard, and the production digest registry in the same change. A new
shared claim-derived view must compare its answer at the same explicit time and recipients.

The [canonical projections audit](canonical-projections-audit.md) records the original gaps and
local exceptions. Modern status reports a graph digest over the complete projection registry,
plus one digest for every table. The original six-table hash remains a compatibility field on
the peer protocol; it cannot prove that every shared outcome agrees.

Shared reducers use `store/canonical.rs`. `canonical_sql` expands `CANONICAL_ASC(ALIAS)` and
`CANONICAL_DESC(ALIAS)` in a query; `CANONICAL_ORDER` and its descending counterpart format the
same order for existing claim queries. In-memory comparisons use `claim_key` or
`key_from_record`, and claim-to-claim predicates use `after_sql`. Legacy batch position is its
relative position within the batch. A global arrival index never chooses a shared winner.

Document version rows retain the earliest canonical binding for repeated name/hash pairs;
mission revision rows do the same for repeated identical revisions. Document latest flags,
history order and cursor boundaries use binding claim keys. Complete mailbox readers and
selected unread reminders use canonical sent-claim keys; bounded mailbox cursors remain local.
The shuffle fixture compares paged document answers and the existing person attention view,
including unread messages, reminder selection and episode onset. Proposal lifecycle tests
compare all shared rows and digests through creation, review, draining, cancellation and apply.

Documents record a sortable encoding of the complete binding claim key at admission.
`document_canonical_latest(name,binding_key DESC)` serves latest selection and history order;
readers never sort a name's entire claim history for each returned version. Schema 15 backfills
the keys once, choosing the earliest canonical binding for each repeated name/hash. The key is
a shared derived column and is digested with the document row. The fleet-size regression builds
260,000 claims and 10,000 document versions, then requires latest listings, history pages,
lookup and cursor reads to finish within two seconds and verifies the indexed latest query plan.

## Rolling upgrades and waiting claims

A registry difference never refuses an exchange. Admission checks the envelope signature,
wire hashes and batch identity before classifying its claims. Kinds and fields this build does
not know remain in the original envelope and in retryable `unknown` receipt records. Known
claims in that envelope and later envelopes from the same writer continue to project. An
unknown claim alone does not degrade its envelope or mark a projection unhealthy.

`st replication status` reports `waiting` claims; JSON exposes `waiting_claims` and retains
`unknown_records` for existing readers. Doctor reports the waiting count without treating it
as a fault. At daemon startup, admission retries those original records using the upgraded
registry, then the canonical projector rebuilds their shared rows. An earlier claim learned
on upgrade must sort by its original canonical key, never its new local arrival index.

The `authority_digest` is the build-independent log digest: it commits every stored envelope
identity, including envelopes containing unprojected claims and checkpoint tombstones. Each
envelope hash commits its original payload and chain metadata. Thus members holding the same
wire log agree on this digest even while their projections differ. Inventory equality does
not prove signature admission; unsigned, fenced and invalid records retain their diagnostics.
The graph and table digests describe this build's admitted projections and may differ until
all members learn the same claim vocabulary. Automatic projection comparisons and heals run
only for matching registry digests with no locally waiting claims. A rolling upgrade clears
old divergence measurements; comparisons resume after upgrade. A first sync between different
builds verifies the equal wire log once every envelope passes admission, recording its
`authority_digest` and leaving graph equality unasserted. Fleet join can then complete while
claims wait for an upgrade. This preserves full projection
checks for members with the same registry without asking an older reader to project new facts.

The signed two-node regression
`store::fleet_admission_tests::mixed_builds_keep_signed_unknown_claims_and_project_them_after_upgrade`
uses one registry without document bindings, exchanges unknown claims and later known claims
in both directions, then reopens the older store with the current registry. It requires equal
log digests before upgrade and equal graph and every table digest after canonical replay.

## Projection digest coverage and cost

`store/projection_digest.rs::TABLES` lists the shared tables: operations, blobs, documents,
desired, message_index, mission_revisions, mission_definitions, mission_runs,
mission_run_deadlines, mission_run_after, run_generations, step_runs, revision_proposals,
planning_sessions, planning_candidates and planning_previews. Every column joins the digest
by default. Only document/message/mission-revision arrival indexes and the effective step lease
expiry/change timestamps are excluded. Those step timestamps include member-local renewals;
the original durable lease facts remain covered through their authenticated claim identity.
`planning_previews.store_index` is an originating preview input and is covered.

The `claim_sources` digest covers admitted claim identities and immutable acceptance times,
including claims represented by checkpoint tombstones. Claim identities commit bodies,
predecessors, actors and batch identity; authenticated envelope inventory commits complete wire
payloads and ordering metadata. This covers the sources of on-demand views such as ownership,
subscriptions, fleet membership, usage, observer/fault episodes, and attention. A timed view's
answer must still be tested at the same explicit time and recipients. Host-local liveness,
leases, receipts, cursors, secrets and notification bookkeeping stay outside shared digests.

A repaired original is no longer an admitted projection source. One member may retain its
old claim row while another rejected it before admission; both exclude it from claim-source
and operation projections. The original wire bytes remain committed by authenticated envelope
inventory, and the repair and replacement remain shared claim sources. A local
`projection_digest_repaired_claims` cache retains the exclusion after receipt cleanup; repair
record updates and cached source digests commit or roll back together. Registry version 5
backfills existing repairs once, and operation rules version 2 rebuilds older operation rows.

Operations are logical rows over the hot operation table and operation facts retained in
checkpoint tombstones. Trimming a claim changes its storage representation, preserving its
logical operation and claim-source digests. The shuffle test compares that logical union.

An idempotency key names an operation. A member that holds a claim for a key answers a retry with
that claim and refuses the key for a different request. Two members apart, as during a partition,
can each accept the same key for different requests: once they meet, both claims stand, the
operation is a `conflict`, a retry with the key is refused as `idempotency-conflict`, and
`st doctor`'s `idempotency-keys` check names the subjects and writers.

SQLite triggers update per-table row counts and 512-bit modular sums of domain-separated
SHA-512 row hashes in the row's own transaction. Insert, update, delete, replacement, savepoint
rollback and commit therefore change rows and cached digest state together. SHA-256 commits the
table name, column schema, count and sum; a sorted map of those table digests commits the graph.
Shared columns are encoded in sorted name order, including the logical operation row. A fresh
schema and an additive migration can place the same column at different physical positions;
that layout does not change the digest. Registry version 6 rebuilds older physical-order caches.
These are diagnostic digests; authenticated envelopes and signatures remain the replication
integrity boundary. Work scales with changed row bytes, plus the fixed-size accumulator.
Reading all table digests reads one small cache, independent of retained history size. Operations
keep a local row cache to update their hot/tombstone union by operation identity. The first open
of a registry/schema version backfills these caches; routine reads and subsequent opens never
rescan projection histories to compute digests.

`incremental_digests_cover_each_shared_column_and_roll_back_with_rows` changes every shared
column and compares cached digests with a full-scan oracle, including local exclusions, no-op
updates, replacement and rollback. The shuffle/restart/checkpoint regression compares every
cached table digest with that oracle in every phase. The table classification guard requires
the test inventory and production registry to agree, and the fixture exercises every table.

`st replication status` prints `table-digest` entries and names differing tables under each
peer. `st replication diff PEER` and `st doctor` also name them. Table differences are meaningful
only when inventories and claim registries agree, no claims wait for a newer build, projection
is current and local committed batches are sealed. Old peers omit `projection_digests`;
exchanges and heals then compare their unchanged six-table compatibility hash. Old peers cannot
verify full projection coverage. Modern peers compare complete maps. SQLite schema 14 adds the
transactional digest machinery and rebuilds shared projections once, correcting older stored
creation/change dates from claim facts. New writer connections must register the projection
functions, including isolated checkpoint proof connections.

The indexed `resource_observations` projection participates in the complete shared table digest
and canonical replay audit. The projection version identifying its layout is included in the
runtime schema compatibility identity alongside the claim vocabulary digest. Mixed builds with and
without this projection continue exchanging admitted claim authority, but do not compare their
incompatible projection maps as if they represented the same graph. After upgrade, the table is
backfilled from retained admitted `resource.observed` claims and projection comparisons resume.
This changes no claim or resource vocabulary and requires no new replication payload fields.


## Add any machine

Install st on the new machine and configure the person who operates it, as described in the
[README](../../README.md#run-the-daemon). On an existing listening member, invite the new name:

```sh
st fleet invite beacon
```

On the new machine:

```sh
st fleet join
st fleet status
st replication status
```

Paste the invitation when asked. These are the same steps for a laptop or server. Join receives
the fleet settings, installs the services, and catches up its replica. It discovers usable
Tailscale and Fabric endpoints; a firewall that permits only outbound connections needs no
extra sync setting. Existing Fabric trust and permission for the sponsor's fleet protocol, or
a Tailscale ACL permitting its worker port, must allow the connection.

To choose the sponsor route explicitly, use its actual tailnet address/port or canonical Fabric
NodeID/protocol. Each command prompts for the same invitation:

```sh
st fleet join --via http://100.64.0.10:31313
st fleet join --via fabric://NODE_ID/PROTOCOL
```

When the machines are already Fabric peers, the invitation can travel as a file instead:

```sh
# On the sponsor:
st fleet invite beacon --via fabric --send-fabric
# On beacon:
st fleet join --fabric-inbox
```

No helper needs to maintain a loopback dial port. The worker obtains Fabric tunnels itself and
uses available alternative member routes. `--dial-out` is optional when you explicitly want
no inbound listener; it does not change retry or presence behavior. See [Fleet join](../fleet-join.md)
for trust, invitation expiry, removal, and the migration of an existing config-peer fleet.

## Legacy configuration and route overrides

A fleet configured with a shared secret and fixed peers remains supported. Its values look like:

```toml
node = "node-a"
fleet_id = "1f91ca65-7793-48cc-866e-ac15690130e1"
shared_secret_file = "/absolute/path/to/fleet.secret"
peer_listen = "127.0.0.1:31313"

[[peers]]
name = "node-b"
url = "http://127.0.0.1:31314"
```

The fleet ID is a persistent UUID. A store rejects another fleet ID after its first binding.

The secret contains 32 raw bytes or 64 hexadecimal characters. Its file mode must deny group and other access.

The peer listener accepts loopback or a literal Tailscale IP in `100.64.0.0/10` or
`fd7a:115c:a1e0::/48`; the worker can also bind discovered tailnet addresses. A listener bound
to a Tailscale IP is advertised as this member's Tailscale endpoint.
Explicit peer routes accept loopback or tailnet HTTP and `fabric://NODE_ID/PROTOCOL`.
Fabric routes name the lasting remote endpoint: the worker creates or reacquires the local
tunnel itself. Several peer entries may name the same member with distinct routes.
Arbitrary DNS names and LAN/public HTTP peer URLs remain rejected. The listener refuses a
wildcard, LAN or public address unless `peer_listen_allow_plain_http = true` (or
`--peer-listen-allow-plain-http`) is set: st cannot tell whether such a path is encrypted,
and the listener carries terminal input and attach grants as well as replication.
Replication is plain HTTP, encrypted between hosts by Tailscale or a Fabric tunnel;
the fleet secret and joined members' signatures still authenticate the exchange.
See [Tailscale setup](tailscale.md) for direct-host setup and
[the migration procedure](../fleet-join.md#move-an-existing-fleet-off-local-dial-helpers)
for replacing a legacy local dial helper.

The peer HTTP listener is not the privileged local Unix API: it exposes only fleet replication,
join, and owner-forwarded client operations. Replication and forwarded client requests require
the fleet HMAC and peer admission; current members must also sign with their member key.
Joining instead requires a valid invitation proof and the joining member's signature.
Binding a tailnet address does not expose the privileged Unix API or bypass these checks.

On a node that only receives connections from a peer, list its name without a URL:

```toml
[[peers]]
name = "node-b"
```

This accepts node-b's authenticated exchanges and never dials it. The equivalent command-line
entry is `--peer node-b`. A peer is observed as up after a successful exchange in either
direction. After 90 seconds without an exchange it is shown as `last-seen`, with the time
of its last successful exchange, rather than as a fault. It is `last-seen` sooner when an attempt
to reach it has failed and it has missed a 35-second quiet exchange interval, as a frozen peer
does; a failure within that interval can be a one-way route while the peer still dials in. The
last failed attempt stays as the peer's `last_error`, with its time, until the next exchange. A
sync measurement older than the quiet interval is marked `stale`, with the envelopes this node
gained since. Doctor names every absent peer and how much this node has not sent it, and warns
only for an absent listening member; a dial-out member or a config peer can be away for hours.
Delivery probes and their attention streaks wait for the member to exchange again.
A config-peer node can omit `peer_listen` and initiate every exchange itself; it still pushes
and pulls the full graph.

### Fleet members

A node that joined or migrated keeps its fleet settings in `STATE/fleet/fleet.toml`, written by
`st fleet` commands, and needs no `[[peers]]`. Its peers come from membership claims in the graph:
it dials every current listening member and accepts exchanges from current members that sign with
their member keys. The existing dial-out mode controls whether the worker opens a listener; it needs no separate
presence policy. Every current member appears in replication status and machine views with its
last exchange time. Members behind NAT can initiate exchanges and converge in both directions,
even while other members' attempts to reach their advertised addresses fail. A `[[peers]]` entry for a member is that member's first route
from this machine. Service units for a member carry no peer, fleet, or secret arguments. See
[Fleet join](../fleet-join.md) for invites, removal, and migration.

## Process boundary

The main daemon owns the local API, projections, reconciliation, and runtime changes.

The `replication-worker` process owns peer HTTP, authentication, exchange, and admission.

The service installer creates a separate systemd user service or launchd agent for the worker. A worker crash cannot stop the main daemon.

A local graph change writes `replication.wake`. The worker watches only this file, not the SQLite files.

The worker coalesces wake bursts for one second, so new authority, such as a mission publish, reaches every peer within seconds. An exchange that stored new envelopes on either side runs again at once until a backlog drains. An exchange that stored nothing new waits for the next wake, even if it carried envelopes the other side already held.

The worker also runs a 30-second anti-entropy exchange. A recent inbound exchange suppresses
a redundant connection in the opposite direction; local graph changes still request a prompt
push. Failed attempts back off exponentially from one second through minutes to one hour, with 20 percent
jitter. Graph wakes do not reset failure backoff. An authenticated inbound request, a successful
peer request, a change to the member's routes, or a local connectivity change interrupts it
immediately, including activity received while an outbound request is still failing. Peer
activity has a separate notification channel used only by failure waits, so ordinary
authenticated traffic does not start redundant healthy exchanges. A returning outbound-only member starts its own
push and pull without waiting for the other members' retry timers. Fabric tunnels are obtained
again on each attempt, so a restarted Fabric does not leave a stale cached tunnel. With Fabric
0.2.21 or later, the worker also consumes its passive `peer-events --watch` stream: an online
admission resets that member's retry, and a daemon reset refreshes exposures and announces this
node. Offline transport events create no fault. Older Fabric keeps using address-change,
suspend-gap, and anti-entropy recovery. Tailnet listeners and advertised endpoints also
refresh on local connectivity changes, without waiting for their minute timer; disappeared
interface addresses stop being advertised.

An HTTP link can return silently without changing either machine's addresses. For five minutes
after a successful outbound exchange, a failed dialer's retry wait probes that same HTTP route
every three seconds, with a one-second timeout. The probe uses `HEAD` on the existing exchange
route: any HTTP response, including an older build's `405`, wakes a signed exchange at once.
It neither exports an inventory nor records a failure. A route that has never worked, a replaced
route, or a peer absent for more than five minutes keeps the long retry schedule. Transport life
only schedules an exchange; the exchange still authenticates every claim and peer.

The same recovery applies to every member. A server may be absent for hours just as a laptop
may be asleep. On daemon start, wake from sleep, network change or Fabric recovery, a member
announces its current endpoints and attempts exchanges with every reachable member. Either
side can initiate; one connection transfers both sides' missing history and repeats until the
backlog drains. The other side need not open a second connection or wait for its retry timer.
An absent member is shown with its last exchange time, and doctor and delivery attention wait
for contact before judging its routes. Invalid signatures, rejected membership and corrupt
records remain faults.

## Protocol

Every HTTP request uses HMAC-SHA256 over the protocol, method, path, fleet ID, node, and body digest.

Every authenticated response uses the same secret. The response signature also binds the request digest.

The secret never enters a request, response, database, or log.

One exchange sends an inventory of envelope identities. An identity is `(writer, sequence, envelope hash)`.

The inventory is a set, not a high-water cursor. Sparse delivery and two candidates at one writer sequence are valid.

The inventory is compact. It carries one digest per writer range of 256 sequences, and it lists identities only for ranges whose digests differ. A peer sends a range the other side lacks entirely, and it sends identities within a differing range only after the other side has listed that range. One two-phase exchange therefore converges both directions without sending the whole authority log. A peer without range digests receives and sends the full identity list, as before.

Each inventory says how many envelopes its sender takes in one exchange, 4,096 for this build. A peer sends at most that many, and at most 512 to a peer whose inventory does not say, as older builds do not.

Each missing envelope contains one base64-encoded CBOR payload. Receipt stores the outer envelope before it decodes the payload.

An exchange body larger than 64 KiB travels deflate-compressed (`Content-Encoding: deflate`), which shrinks a page of envelopes to about a third. A requester asks for compressed answers with `Accept-Encoding: deflate`; a peer's answer carries the same header when it takes compressed requests, and the requester then compresses its push. Signatures cover the uncompressed JSON, and an inflated body may not exceed the 64 MB exchange limit. An older build neither asks nor says, so it exchanges plain JSON.

The payload is only an immutable claim batch plus the content-addressed blobs those claims
reference. Nodes do not send SQLite rows, leases, reducers, projections, or runtime snapshots.
Each receiver derives projections locally from the admitted claims.

## Four durable stages

1. Receipt verifies transport authentication and stores each new envelope.
2. Admission verifies the envelope, batch, blobs, claims, and current schema.
3. Projection reduces valid claims into the current local graph.
4. Reconciliation changes local runtimes from the last good graph.

An unknown or invalid record stays in `replica_records`. It does not block a valid sibling or a later envelope.

Admission commits once per pass over the pending envelopes, so one disk flush covers an exchange. Each envelope is admitted in its own savepoint, so an invalid one is rolled back and recorded alone.

A node catching up with a peer, one more than one exchange behind it, projects at most every 30 seconds and again as soon as it has caught up. History arrives older than the node's own claims, so the node cannot extend its projection incrementally; projecting after every exchange would replay the whole graph each time. Meanwhile `st now`, `st missions ls` and stui say the node is syncing.

Person-work asks, completions and cancellations rebuild only their owning mission run tree.
Standalone asks embed their run and generation in the asking claim; indexed lookups recover
that ownership even before the ask is projected. A response received before its ask is replayed
with the ask in canonical order once both arrive. These updates preserve unrelated runs and
avoid holding the single writer for a full retained-graph replay. New claims find their canonical
predecessor by seeking the newest time block of their subject, then resolving writer, sequence
and position ties within that block. They do not sort the subject's whole retained history.
Regression tests compare all
17 shared maps with full replay and the digest scan after reopen, and exercise receipt and
admission while normal claims and worker renewals write to a populated WAL database.

An unknown claim kind or field can become valid after a schema upgrade. Admission retries unknown records on each wake and startup. Records that older builds classified as invalid solely because of an unknown field are also reconsidered, preserving and admitting the original signed claim when the upgraded schema recognizes it.

A projection fault keeps the last good projection. The daemon continues to serve status and repair commands.

A valid claim can still fail to project, for example a body written by another build. That claim
is quarantined on its own: its projection is rolled back, and every other claim, from every peer,
still reaches the graph. `st replication status` lists it as an `unhealthy` projection named
`projection:base:CLAIM` or `projection:runs:CLAIM`, and `st doctor` names it. Each full replay
decides the claim again.

A repair that cannot be applied is listed the same way, as `repair:CLAIM`. The other repairs still
apply, and the daemon still starts.

## Deterministic convergence

Every node preserves every authenticated envelope candidate.

The authority digest sorts writers, sequences, hashes, and payloads. Receipt order cannot change this digest.

Reducers use causal ancestry where it exists. They use stable claim data as the final concurrent tie-breaker.

Terminal revision proposal states do not regress during replay. Local rules still allow only one pending proposal for a mission run.

A full replay starts from nothing, as a node that joins late does. It clears the graph tables,
replays every claim in canonical order (accepted time, writer, batch sequence, position in the
batch), and then applies local lease renewals again. Replaying over the rows an earlier projection
left behind made the graph depend on how that projection ran: it could apply a run's old terminal
state over the revision that reopened the run, so two nodes holding the same claims showed
different graphs. A rule that reads other claims finds them by subject, never by the batch it
arrived in, since a writer may put related claims in separate batches. The replay reads every
claim before it projects any, so a claim that fails to project is quarantined without ending the
replay.

A replay from nothing is not the normal path. A projection extends the graph with the claims it
admits. A claim that reaches part of the graph out of the replay's order rebuilds only that part
from its own claims, in the replay's order and passes. The parts are:

- a mission run tree, which is a root run with its child runs, generations, steps, work and
  revision proposals;
- one desired subject;
- one document;
- one mission.

So does every run tree that holds a claim this node wrote since the last projection, because this
node applied that claim when it wrote it. A claim whose run or generation has not arrived waits in
the claim log, and the claim that creates the run or generation rebuilds its tree. A revision that
arrives after a run, generation or proposal that uses it rebuilds those trees. Claims of every
other kind only add events and operations, whose order does not matter.

The graph is replayed from nothing only in these cases:

- a stale or unhealthy projection;
- an operation whose digests conflict;
- an operation claim without a digest;
- a work claim that carries an operation;
- a heal;
- the first start of a build with the rule below.

A rebuilt part costs what its own history costs, while a replay holds the store's only writer for
as long as the whole graph takes.

Routine work actions, including `work.extended`, project incrementally. An extension preserves
the attempt's execution budget and lease on its own node and its peers; an out-of-order extension
can rebuild its run tree. The extension regression checks newer local state, full-replay parity
and restart without requiring a full-store replay for the work action.

The extension correction uses checkpoint rules v6 and a new shared projection compatibility
identity. Mixed builds continue authority sync and defer incompatible graph comparisons. On
upgrade, `work_extended_projection_rules=1` records a successful canonical rebuild of just the
roots with retained extension claims; unresolved roots leave the migration pending. Existing v5
certificates and tombstones remain historical authority, and their certified manifests can still
be adopted. All checkpoint participants must use v6 before the next cut verifies.

Incremental catch-up projection commits at most 128 newly admitted claims per transaction and
returns the writer between chunks, so queued messages and lease renewals can run. Each commit
records the frontier actually projected. Admissions and catch-up deferrals advance a generation;
the final pass clears only the generation it observed while holding the writer, so it cannot
overwrite a newer deferral. Sync comparisons remain deferred until the pass has
reached its starting backlog and no later admitted claims remain. A dirty aggregate still
rebuilds from its complete canonical history; the chunk limit does not bound that history or a
fallback replay. The regression interleaves local writes with committed prefixes and checks the
final graph against canonical replay.

The first start of a build with this rule replays from nothing once. A run that this node created
could show as over in its old graph while its claims say it runs. Starting that work again long
after anyone expected it would surprise people, so the node writes the claims that end the run as
its graph showed it, and says so at startup. It does this only while no peer claim on the run
waits to be projected, since such a claim may have reopened the run for real. Runs that other
nodes created are theirs to settle.

## Inspection and repair

Use these commands:

```sh
st replication status
st replication diff node-b
st replication invalid
st replication inspect record/HASH
st doctor
```

The status view reports the authority digest, graph digest, record counts, projection health, and last peer results.

A Fabric `peer not permitted for service` answer means the member's grants refuse that direct
route. It shows as `refused`, with the Fabric node and service, in `st replication status`,
`st fleet status` and `st doctor`. It is neither an absent member nor a replication error;
`last_error` is empty and the JSON status carries `refusal_reason`. Doctor's replication check
passes when the refusal is the only issue.

The worker retries that route every 24–36 minutes, at most three attempts per hour including
the first refusal. Local writes, inbound exchanges, Fabric online events and local network
changes do not shorten this delay. A changed route retries immediately, and a later successful outbound
exchange clears the refusal. Other routes to the same member remain usable. Members that do
not grant each other the service can still converge through a common hub; the hub must grant
and exchange with both leaves. The worker never changes Fabric grants itself.

For each peer, the status view also reports when the last exchange happened and how far apart the
two envelope sets were at that exchange:

```text
sync	catching up: node-b has 124,384 envelopes this node lacks, caught up in about 15m
peer	node-b	up
  last exchange 2s ago
  node-b has 124,384 envelopes this node lacks
  this node has 3 envelopes node-b lacks
  receiving 142.5 envelopes/s, caught up in about 15m (measured 2s ago)
```

Each node measures the difference from the inventory its peer already sends in every exchange, so
the protocol does not change and a peer on an older build is measured too. A range both sides hold
with different digests counts exactly once the peer lists it; until then its count is a lower
bound. The estimate divides the remaining envelopes by how fast that number shrank over recent
10-second windows, so a peer that keeps writing lengthens it. The measurements live in memory; the
first exchange after a restart rebuilds them.

The `timings` line shows where this daemon has spent replication time since it started:

```text
timings	486 exchanges, 130004 envelopes received; ms: round-trip=6088 export=340 snapshot=2775 receipt=1591 admission=33634 (verify=1971) projection=489104 repair=12 signing=0 sqlite=566278 (132887 commits, 17587 ms)
```

Each store stage counts only the time it holds the store's write connection, so one stage does
not count another's wait. Round trips are this node's own requests to its peers, including the
peer's work to answer them. SQLite time is every statement the daemon ran; each commit waits for
a disk flush. `/v1/replication/status` carries the same numbers as `timings`.
`crates/st3/tests/first_sync.rs` uses them to profile an empty node syncing from a peer; `cargo test --release -p st3 --test integration first_sync:: -- --nocapture` runs it.

A node is catching up while a peer measured in the last five minutes holds more envelopes than one
exchange carries. During that time its projections can show early history as current: a request
that a later envelope resolves still looks open. Every client page then carries a `sync` notice,
`st now` and the other product commands print a `SYNCING` line before their items, and stui shows
`⟳ Syncing` with the same line.

Two nodes are in sync only when they hold the same envelopes and project the same graph from
them. Each exchange at which both nodes hold the same envelopes compares their graph digests; an
exchange that stores new envelopes, or meets a deferred projection, compares nothing. The same
envelopes must project the same graph, so a difference that outlasts a minute, longer than a peer
takes to project what it stored, means the graphs diverged: for example, one node's projection
followed a rule that a replay from nothing does not, or lost claims it had admitted while keeping
their envelopes. Exchanges cannot fix that, so the status view leads with it:

```text
sync	diverged: node-b holds the same envelopes but projects a different graph, since 3m ago
peer	node-b	up
  last exchange 2s ago
  diverged: the same envelopes project different graphs since 3m ago (compared 2s ago; this node 0f3a9c21d4e8, node-b 7b21e05c9a44)
  exchanges cannot fix this; the nodes heal by comparing the claims each projects, and views on one node are wrong until then
```

While any peer has diverged, `st doctor` fails its replication check, every client page carries a
`sync` notice in the `diverged` state, `st now` and the other product commands print a `DIVERGED`
line, and stui's header shows `⚠ diverged`. A shorter difference shows as `graphs differ` and fails
nothing. A comparison stands until the next exchange at which both nodes hold the same envelopes;
the first one that finds equal graphs clears it. Like the envelope difference, comparisons live in
memory and the first exchanges after a restart rebuild them.

### Heal

Diverged nodes heal without a person and without a reset. When the replication worker's own
exchange with a peer finds the graphs still different after a minute, the worker starts a heal
over the signed peer route `/v1/peer/heal`. The node that dials asks the questions, and the main
daemon on each side answers from the claims it projects:

1. **Ranges.** The digest of the claims each node projects from each writer range (the same
   bucket ranges the envelope inventory uses).
2. **Subjects.** For each range that differs, the digest of its claims about each subject.
3. **Claims.** For each subject that differs, the claims themselves and the envelopes that carry
   them.
4. **Swap.** Each side sends the envelopes that carry claims the other lacks. The receiver checks
   each envelope against its hash and admits it again through the usual validation, repair, and
   projection. Claims move in both directions in one swap, and a heal repeats the narrowing while
   swaps still move claims.

When both nodes project the same claims but different graphs, their projections differ, and a
replay from nothing decides the graph: first the asking node replays its own, then it asks the
peer to replay. A replay holds the store for as long as it takes (about 40 seconds on a 2 GB
store), so a node replays for heals at most once every 10 minutes, and backs off to once a day
after replays that did not make the graphs agree. A heal that changed nothing waits twice as long
before the next, up to an hour. A peer on a build without the heal route answers with an unsigned
404, and the heal reports that.

`st replication status` shows the last heal with each peer:

```text
peer	node-b	up
  last exchange 2s ago
  in sync: the same envelopes and the same graph (measured now)
  healed 1m ago: admitted 14 claims node-b projects (1 ranges and 3 subjects differed); the graphs agree
```

A heal that could not make the graphs agree says why, for example `node-b cannot admit 2 claims:
2 unknown (unknown-claim-kind) that this node projects`, and the pair stays `diverged` until a
later heal succeeds.

### First sync

A machine that joins with `st fleet join` records a first sync. It ends at the first exchange at
which the new machine holds the same envelopes as a peer. That exchange compares the two graph
digests at once, without the minute a running node allows a peer to finish projecting, and a
difference starts a heal immediately. The first sync is then `verified`, possibly after a heal,
or `failed` with the heal's reason. `join` waits for it by default and fails when it fails;
`st fleet wait` waits for it later. A failed first sync fails `st doctor`'s replication check,
and `st replication status` prints it as a `first-sync` line with both digests.

A verified first sync says what was true then. `st fleet wait`, the gate after a restart or a
deploy, also waits for an exchange since it began, with every peer that is up, at which this node
held every envelope that peer held; peers that are not up are named as not checked. Doctor warns
while a peer says this node is catching up, and while the daemon has not exchanged with any peer
since it started.

Repair publishes a new claim. It does not delete or change the bad record.

```sh
st replication repair record/HASH \
  --with CLAIM_ID \
  --reason "replace the invalid observation" \
  --as person/operator
```

The same repair is declarative KDL:

```kdl
version 2

repair "record/HASH" {
  replacement "CLAIM_ID"
  reason "replace the invalid observation"
}
```

A replacement must already be a valid admitted claim. The original record remains visible with state `repaired`.

## Checkpoints

A checkpoint deletes replicated claims that no longer change any answer. Checkpoint
`checkpoint/D` covers claims dated before the start of UTC day `D`, its cut, and becomes due at
the start of day `D+2`. Each daemon does the checkpoint work that is due every ten minutes.

A checkpoint drops only claims of these kinds, and only when a later claim of the same slot that
it keeps sets every field a reader takes from the dropped one:

- `harness.observed`, `harness.timeline`, `loop.state`, `observer.observed`,
  `transport.observed`, `daemon.diagnostic` and `subscription.mission-deferred`;
- `render.applied`, `runtime.readiness-deadline-reached`, and the runtime's own
  `runtime.action.*` claims;
- `harness.usage` snapshots. A response rollup series keeps the last snapshot of each UTC hour,
  by the time its writer measured it, for seven days before the cut, and only its newest snapshot
  before that. `st usage` stays exact to the hour for a period inside that window, a longer period
  counts each series from its start, and a series' total never changes. A session's cumulative
  reading keeps its newest and its largest; a context reading keeps its newest. Per-response
  claims from older builds stay;
- `harness.limits` readings, keeping each seat's newest.

Every model response also goes to OpenTelemetry when `[observations.otlp]` is set: an
`st.usage.response` log with its agent, mission run, step, model, account, tokens and cost, and
the counters `st_usage_tokens_total` and `st_usage_cost_microusd_total`, labelled only by
driver, model, account and cost basis. That export is the history beyond the window.

It never drops a claim a person wrote or a claim another claim cites as evidence. Mission run,
step, membership and message claims are never dropped. Each node proves the drops before it agrees to
them, and every node deletes the same ones:

1. **Seal.** Each participant publishes `checkpoint.sealed`, naming the envelopes it holds from
   before the cut. The participants are every writer the node has heard of, less those that left
   the fleet and those a person excused.
2. **Verify.** Once every participant has sealed the same envelopes and rules, each node plans
   the drops from the claims of those envelopes, less repaired originals: whether a node holds a
   repaired original depends on whether it admitted it before the repair arrived. It proves on a
   copy of its store, cut down to those claims and the blobs they reference, that deleting them
   changes neither the graph nor any reader's answer, then
   publishes `checkpoint.verified` with the digests of the drops, the kept claims, the graph and
   the answers. A node verifies a checkpoint once.
3. **Trim.** When every participant verified the same digests, the checkpoint is stable. Each node
   first records a tombstone for every envelope and claim it drops, then deletes them in chunks.
   After a crash, the next pass finishes the trim. The tombstones stand in for the dropped
   envelopes in the inventory. The authority digest does not change, and peers, including builds
   without checkpoints, never send those envelopes again.

[Checkpoints and trimming](checkpoints.md) explains the rules, with worked examples.

A node that did not verify a stable checkpoint, such as one that joined later, adopts it. A peer
lists the newest checkpoint it trimmed in its inventory. The node fetches the manifest of
tombstones from that peer and checks the whole manifest against the verified drop digest before
it stores anything. Then it trims to the same tombstones. A heal never fetches back what a
checkpoint dropped.

A participant that does not seal holds up every checkpoint, whether it is away or runs a build
without checkpoints. Three days after a checkpoint became due, a participant asks a person in
`st now` to bring the machine back or excuse it:

```sh
st replication checkpoint status                  # the newest stable checkpoint, and who sealed the next
st replication checkpoint plan --cut 2026-09-27    # what it would drop here, proved on a copy
st replication checkpoint excuse node-c --reason "away for a week" --as person/operator
```

An excusal fences nothing. What the excused writer wrote while away still replicates when it
returns, and its next seal ends the excusal.

Excusing each side of a partition lets each side certify on its own. When they meet again, every
node applies the same certificate: for one cut, the one with the most participants, and
otherwise the newest. Adopting its manifest makes a node's tombstones exactly the manifest's. A
claim that only the other side saw and dropped is then forgotten: no node holds it or its
tombstone. Only a claim a rule may drop can be forgotten, one a kept claim replaces, so every
answer stays the same, and the nodes still agree with each other. Person and mission claims are
never dropped, so they are never forgotten. Issue #1052 tracks keeping those tombstones too.

When verifications differ, that checkpoint never becomes stable, and the next due checkpoint
tries again. `status` names which digests differ for each participant. Seals that differ in
`rules` mean the nodes run builds with different checkpoint rules; they wait until every
participant runs the same rules.

If a trim finds that a deletion would change the graph, it rolls that deletion back and stops. It
records a `checkpoint-trim-graph-changed` diagnostic, and the node seals nothing more until a
person runs `st replication checkpoint resume --reason ...`.

Checkpoints are on by default. `[checkpoint] enabled = false` in a node's config stops that node
sealing. Every participant must seal, so this stops trimming for the whole fleet.

## Recovery

Replication never changes a source SQLite file directly. It exchanges immutable authority records through the signed protocol.

After a partition, each node continues local work. A later exchange transfers every missing candidate in both directions.

If two nodes differ, inspect unresolved records and peer errors first. Publish a replacement repair when a record needs correction.

Do not copy one live SQLite file over another. A file copy can discard concurrent authority and local runtime state.
