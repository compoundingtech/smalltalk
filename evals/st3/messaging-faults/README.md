# Messaging faults

`scripts/st3-messaging-faults-eval/run ST3_BIN OUT --old-binary OLD_ST3_BIN` exercises
the sender-to-reader path across two isolated nodes. It uses real daemon processes,
signed fleet replication, native omp drivers, channels, and the extension embedded in
each tested binary. Node.js supplies a small provider API stand-in that loads that
extension and consumes `sendUserMessage`. No model or LLM judge runs, and the reader
never polls a mailbox. This measures the transport and harness integration boundary;
it does not prove a real model's response time or every harness's integration.

Run outside an st seat, since the sender acts as `person/eval`. For example:

```sh
setsid -f env -i HOME="$HOME" USER="$USER" PATH="$PATH" \
  XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
  DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" \
  scripts/st3-messaging-faults-eval/run "$ST3_BIN" /tmp/messaging-faults-run \
  --old-binary /path/to/pre-reexec/st3 \
  >/tmp/messaging-faults-run.log 2>&1 </dev/null
```

Python 3.11+, Linux `/proc`, `pty`, and Node.js with native TypeScript support are
required. Pass through the user service manager environment to give isolated seats
their own scopes. Each daemon has private state, a private Unix API socket, a
private PTY registry, and an invented node name. Peer listeners bind only loopback.
Both peer routes pass through a TCP proxy whose drop closes existing connections
and refuses new traffic. Cleanup stops reconciliation before closing the private
PTY registry. Shared daemons and seats are never stopped or changed.

Every case starts fresh nodes and proves a cross-node warmup before injecting the
fault. The cases are:

| Case | Injection | Fault clears when |
| --- | --- | --- |
| baseline | No fault | Sender accepts the message |
| daemon-restart | Queue remotely before stopping the receiving daemon and replication worker, using a temporary link gate to prevent a successful handoff race | Restarted API answers and worker is started |
| binary-swap | Queue during a partition, atomically replace the receiver's executable at its watched path, restart its daemon/worker | API answers and peer link opens |
| link-seconds | Drop both peer routes for three seconds with a message queued | Both routes open |
| link-minutes | Drop both peer routes for two minutes with a message queued | Both routes open |
| receiver-down | Receiving daemon and worker are down when the sender accepts the message | Restarted API answers and worker is started |
| harness-restart | Kill the provider, queue a message, let the daemon restart its seat | New provider process exists |
| channel-killed | Kill the live channel and send a message | Kill completes and sender accepts the message; recovery belongs to the real extension |
| old-channel | Start a real historical channel under the candidate's driver and extension, then deploy the candidate while queueing remotely | New daemon answers and peer link opens |

Receiver-down models an unavailable receiving node's messaging services with its
seat still alive. It does not simulate a host reboot. Harness-restart necessarily
permits the one injected provider replacement; every other case requires the same
provider PID and runtime incarnation. No case permits an additional provider start.
The old binary must actually predate reexec; the runner records its hash, checks
that it does not support `resume-probe`, and starts its actual channel process
rather than simulating legacy requests. The candidate's driver and extension remain
current, so the case isolates compatibility with an old channel. It does not prove
an old driver's recovery or compatibility between arbitrary historical releases:
failure of that case's warmup is a fixture error, not a fault verdict.

The provider matches consumed text to immutable IDs from real channel-frame
metadata, which also works with old channels that lack message envelopes. Observed
wire frames alone never count as consumption. It writes every native handoff before posting its recipient `delivered`,
`read`, and `closed` acknowledgements. It retries those acknowledgements through
an API outage using stable idempotency keys. The oracle requires exactly one native
handoff, one authoritative graph read within ten seconds of clearing, convergence
of the read receipt back to the sender, and no unexpected provider replacement.
It observes a fixed 25-second tail after clearing, even when the first read is fast,
to catch duplicates and to outlast the delivery report's 20-second startup grace.
Duplicates after that bounded observation window are not covered.

Delivery and reporting are separate results: a working old channel that st reports
as stale fails the reporting gate even if the message arrives. `result.json` records
both receipt timing and the delivery assessment. Per-case evidence keeps graph
traces, native receipts, process snapshots, agent cards, replication status and
daemon/worker logs. Paths are normalized before publishing; stores, configuration
secrets and credential profiles are not copied.

The default swap appends a marker to a copied executable, producing a different
image at the same path. `--swap-binary` tests a real other build instead. `--cases`
selects cases for diagnosis. `--short-seconds`, `--long-seconds`,
`--observe-seconds`, and `--ready-seconds` adjust durations without relaxing the
ten-second read gate. The runner refuses a short outage below two seconds, a long
outage below two minutes, or an observation tail below 25 seconds. It exits nonzero
for a failed gate or fixture error and writes partial results after each case.

The oracle's negative controls run with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover \
  -s scripts/st3-messaging-faults-eval -p 'test_*.py'
```
