# st3 fleet replication

Fleet replication is optional. A node without peer configuration is a complete local-only st3 system.

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

## Process boundary

The main daemon owns the local API, projections, reconciliation, and runtime changes.

The `replication-worker` process owns peer HTTP, authentication, exchange, and admission.

The service installer creates a separate systemd user service or launchd agent for the worker. A worker crash cannot stop the main daemon.

A local graph change writes `replication.wake`. The worker watches only this file, not the SQLite files.

The worker also runs a 30-second anti-entropy exchange. This timer repairs a missed file event or a network interruption.

## Protocol

Every HTTP request uses HMAC-SHA256 over the protocol, method, path, fleet ID, node, and body digest.

Every authenticated response uses the same secret. The response signature also binds the request digest.

The secret never enters a request, response, database, or log.

One exchange sends an inventory of envelope identities. An identity is `(writer, sequence, envelope hash)`.

The inventory is a set, not a high-water cursor. Sparse delivery and two candidates at one writer sequence are valid.

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
st3 replication status
st3 replication diff node-b
st3 replication invalid
st3 replication inspect record/HASH
st3 doctor
```

The status view reports the authority digest, graph digest, record counts, projection health, and last peer results.

Repair publishes a new claim. It does not delete or change the bad record.

```sh
st3 replication repair record/HASH \
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
