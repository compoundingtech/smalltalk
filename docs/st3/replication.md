# st fleet replication

Fleet replication is optional. A node without peer configuration is a complete local-only st system.

Replication makes the logical authority equal across configured nodes. It does not make the SQLite files byte-identical.

## Configuration

Each fleet node needs these values:

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

The listener and every peer URL must use loopback. Fabric or a similar local port exposer carries traffic between hosts.

On a node that only receives connections from a peer, list its name without a URL:

```toml
[[peers]]
name = "node-b"
```

This accepts node-b's authenticated exchanges and never dials it. The equivalent command-line
entry is `--peer node-b`. A peer is observed as up after a successful exchange in either
direction; it becomes down only after 90 seconds without a success. The worker checks peers
without URLs once a minute. Repeated checks do not write repeated transport claims.

### Fleet members

A node that joined or migrated keeps its fleet settings in `STATE/fleet/fleet.toml`, written by
`st fleet` commands, and needs no `[[peers]]`. Its peers come from membership claims in the graph:
it dials every current listening member and accepts exchanges from current members that sign with
their member keys. A dial-out member accepts no connections, is never dialed, and neither records
nor receives transport observations. A `[[peers]]` entry for a member is that member's first route
from this machine. Service units for a member carry no peer, fleet, or secret arguments. See
[Fleet join](../fleet-join.md) for invites, removal, and migration.

## Process boundary

The main daemon owns the local API, projections, reconciliation, and runtime changes.

The `replication-worker` process owns peer HTTP, authentication, exchange, and admission.

The service installer creates a separate systemd user service or launchd agent for the worker. A worker crash cannot stop the main daemon.

A local graph change writes `replication.wake`. The worker watches only this file, not the SQLite files.

The worker coalesces wake bursts for one second, so new authority, such as a mission publish, reaches every peer within seconds. An exchange that stored new envelopes on either side runs again at once until a backlog drains. An exchange that stored nothing new waits for the next wake, even if it carried envelopes the other side already held.

The worker also runs a 30-second anti-entropy exchange. This timer repairs a missed file event or a network interruption.

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
`crates/st3/tests/first_sync.rs` uses them to profile an empty node syncing from a peer; `cargo test --release -p st3 --test first_sync -- --nocapture` runs it.

A node is catching up while a peer measured in the last five minutes holds more envelopes than one
exchange carries. During that time its projections can show early history as current: a request
that a later envelope resolves still looks open. Every client page then carries a `sync` notice,
`st now` and the other product commands print a `SYNCING` line before their items, and stui shows
`⟳ Syncing` with the same line.

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

## Recovery

Replication never changes a source SQLite file directly. It exchanges immutable authority records through the signed protocol.

After a partition, each node continues local work. A later exchange transfers every missing candidate in both directions.

If two nodes differ, inspect unresolved records and peer errors first. Publish a replacement repair when a record needs correction.

Do not copy one live SQLite file over another. A file copy can discard concurrent authority and local runtime state.
