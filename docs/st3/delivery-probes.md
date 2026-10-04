# Token-free delivery probes

`scripts/st3-delivery-probe` runs as a dedicated durable seat on each fleet node.
It sends a real message from its local daemon to every other configured node every
three minutes. The recipient consumes the envelope from the installed binary's
actual `omp-channel` driver and records `delivered`, `read`, and `closed` claims.
It launches no model provider and never polls a mailbox to manufacture a read.
Python 3.9 or newer and the installed `st3` binary are the only dependencies.

Each direction measures one active message. An accepted unread message becomes
overdue after 60 seconds and raises one attention item. At the next probe interval,
the sender creates a fresh message and retains that alert until a recipient's real
read claim proves recovery. A send whose acceptance is uncertain keeps retrying
its original idempotency key. Completing the mission that installed the probe does
not close an unresolved delivery alert.

Boots and channel reconnects hold earlier mail, including old probe messages.
Those messages stay in the mailbox and contribute to the aged unread count
in doctor and stui. Preview and close that backlog with the commands in
[seat deploys](seat-deploys.md), including
`st conversations cleanup --all --older-than 1h`. Cleanup does not count as a
successful native probe; recovery still requires a fresh recipient read.

Sender state, received envelopes awaiting acknowledgement, and send idempotency
keys are written and synced before the corresponding network mutations. The seat
restarts through st, and the consumer reconnects a lost native channel itself.
Receipt replay accepts a message that already advanced only after verifying its
graph state; a closed message without the recipient's read claim cannot pass.
The state directory must remain stable across upgrades. A process lock prevents
two consumers from sharing one directory.

Latency is measured at the sender from its send attempt until it observes the
recipient's replicated read claim. This includes acknowledgement replication and
is an upper bound on the time to read. Running processes use a monotonic clock;
after a restart the pending attempt uses its saved wall time. The result also
records the recipient's accepted read timestamp, but does not subtract clocks
from different machines. `delivered` without `read` never passes the probe.

The latest results and a heartbeat are replicated as immutable document versions
under `doc/delivery-probes/NODE`, every 30 seconds and on state changes. `st doctor`
shows each direction's latest message, measured latency or pending age, overdue
results, missing configured source nodes, and heartbeats older than 90 seconds.
A reported success must match the actual sender, recipient and accepted read
claim in the graph; a document alone cannot manufacture a successful read.
A late receipt remains visible until the next probe completes successfully.
An installed probe that stops is therefore reported as stale, not healthy.
Without any probe documents, the doctor check is absent.

## Configure a node

Install the script into a stable location and create an existing workspace and a
private state directory. Use the following JSON as a template for node `amber`:

```json
{
  "host": "amber",
  "agent": "agent/probe/delivery/amber",
  "state_dir": "/srv/example/delivery-probe/state",
  "reviewer": "person/operator",
  "interval_ms": 180000,
  "deadline_ms": 60000,
  "peers": [
    {"host": "cobalt", "agent": "agent/probe/delivery/cobalt"},
    {"host": "indigo", "agent": "agent/probe/delivery/indigo"}
  ]
}
```

The daemon supplies `ST_AGENT`, `ST3_BIN`, and `ST3_ENDPOINT` to its declared
seat. Optional `binary` and `socket` JSON fields override the latter two, for an
isolated fixture. Declare and preview the dedicated seat before applying it:

```kdl
version 2
agent "probe/delivery/amber" {
  host "amber"
  workspace "/srv/example/delivery-probe"
  restart "always"
  argv "/usr/bin/python3" "/srv/example/st3-delivery-probe" "--config" "/srv/example/delivery-probe/config.json"
}
```

```sh
st agents apply probe.kdl --as person/operator
st doctor --json
```

`agents apply` previews the intent and refuses unresolved references before
publishing it. Create the corresponding configuration and seat for every node, with its other
two nodes as peers. Host and agent identities must agree across configurations.
An offline node remains configured so its missing source heartbeat and unread
inbound messages stay visible. No shared daemon or existing model seat needs to
restart to install these separate probe seats. Remove a probe explicitly with
`st agents stop AGENT --as person/operator`; deleting its declaration alone does
not stop it. Retained probe documents keep that stopped node visible to doctor.

The probe exercises graph sends, fleet replication, native-channel polling,
framing, handoff and read acknowledgement. Its dedicated Python consumer takes
the place of a model harness, so these results do not prove that every live
model's extension or UI is consuming messages. The separate messaging fault
matrix exercises the actual omp extension without model calls.

## Proof

`scripts/st3-delivery-probe-test --binary /path/to/st3` tests a lost send response,
stable idempotency across retry, crash replay of pending receipts, a delivered
claim without a read, a replay after a competing native acknowledgement, a closed
message without a read, incorrect read actors, attention deduplication and recovery.
Its isolated two-node test uses the real native channels and replication, stops
only its own test recipient, observes an overdue attention item and a doctor
warning, then restores that recipient and verifies exactly one read claim.
Shorter test intervals make the outage proof bounded; production uses the
60-second deadline. The native proof runs in the normal Linux Cargo test suite.

A member shown as last seen pauses its route: the probe queues no new sends, withdraws
route attention, and retains any pending message. After a new replication exchange,
it resumes that same message with a fresh measurement window. Doctor waits for absent
members before judging missing heartbeats or overdue routes.

A direct route refused by a member's Fabric grants does not count as a delivery failure.
While another peer is exchanging, the probe continues sending real messages to the refused
member through the fleet, and doctor checks the actual recipient reads and heartbeats.
An indirect delivery that exceeds the deadline still warns. If no replication peer is up,
the probe pauses and retains the pending message until a path returns.
