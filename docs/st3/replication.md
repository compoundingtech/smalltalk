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

The worker coalesces wake bursts for one second, so new authority, such as a mission publish, reaches every peer within seconds. An exchange that moved envelopes runs again at once until a backlog drains.

The worker also runs a 30-second anti-entropy exchange. This timer repairs a missed file event or a network interruption.

## Protocol

Every HTTP request uses HMAC-SHA256 over the protocol, method, path, fleet ID, node, and body digest.

Every authenticated response uses the same secret. The response signature also binds the request digest.

The secret never enters a request, response, database, or log.

One exchange sends an inventory of envelope identities. An identity is `(writer, sequence, envelope hash)`.

The inventory is a set, not a high-water cursor. Sparse delivery and two candidates at one writer sequence are valid.

The inventory is compact. It carries one digest per writer range of 256 sequences, and it lists identities only for ranges whose digests differ. A peer sends a range the other side lacks entirely, and it sends identities within a differing range only after the other side has listed that range. One two-phase exchange therefore converges both directions without sending the whole authority log. A peer without range digests receives and sends the full identity list, as before.

One exchange carries at most 512 envelopes in each direction.

Each missing envelope contains one base64-encoded CBOR payload. Receipt stores the outer envelope before it decodes the payload.

The payload is only an immutable claim batch plus the content-addressed blobs those claims
reference. Nodes do not send SQLite rows, leases, reducers, projections, or runtime snapshots.
Each receiver derives projections locally from the admitted claims.

## Four durable stages

1. Receipt verifies transport authentication and stores each new envelope.
2. Admission verifies the envelope, batch, blobs, claims, and current schema.
3. Projection reduces valid claims into the current local graph.
4. Reconciliation changes local runtimes from the last good graph.

An unknown or invalid record stays in `replica_records`. It does not block a valid sibling or a later envelope.

An unknown claim kind or field can become valid after a schema upgrade. Admission retries unknown records on each wake and startup. Records that older builds classified as invalid solely because of an unknown field are also reconsidered, preserving and admitting the original signed claim when the upgraded schema recognizes it.

A projection fault keeps the last good projection. The daemon continues to serve status and repair commands.

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
