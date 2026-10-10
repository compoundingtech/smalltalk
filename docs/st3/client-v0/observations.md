# Phone observations

`observations.report` posts `ObservationReport` to `POST /v1/client/observations`.
Any current authenticated paired session may report, including a read-only pairing.
Unpaired local Unix sessions, anonymous callers, expired and revoked pairings are refused.
These are optional local diagnostics, outside graph actions, SQLite mutation and replication.
No app recorder, transport setting, activation or collector is implemented by this command.

The generated methods are Rust `observations_report`, Swift `observationsReport` and
TypeScript `observationsReport`. The response is `ObservationResponse`:
`{api_version:"st3.client.v0",request_id:"request/…",value:{accepted:true}}`.
It has no snapshot, fence, action receipt or replicated ACK. Acceptance means a local OS
file write and process-memory admission completed; it does not fsync or promise survival of
power loss. Retry after lost responses with the identical payload and UUID on the same member.

A report has a canonical lowercase nonzero UUID `report_id` and 1–32 samples.
Each sample uses one of two discriminators:

```json
{
  "report_id": "019a1234-0000-7000-8000-000000000001",
  "samples": [
    {
      "kind": "latency", "target": "ios-connect", "carrier": "lan",
      "interval_start": "2026-10-10T15:00:00Z",
      "interval_end": "2026-10-10T15:01:00Z",
      "count": 100, "over_target": 1, "max_ms": 2000,
      "buckets": [[103, 99], [2047, 1]]
    },
    {
      "kind": "live-share", "target": "ios-live-share", "carrier": "lan",
      "interval_start": "2026-10-10T15:00:00Z",
      "interval_end": "2026-10-10T15:01:00Z",
      "foreground_ms": 10000, "live_ms": 9900
    }
  ]
}
```

Carrier is `fabric`, `tailscale` or `lan`; optional `path` is `direct` or `relay`.
Intervals must be exactly one closed minute with minute-aligned UTC `Z` timestamps.
The oldest permitted start is seven days before admission; end may be at most 120 seconds
in the future for clock skew. Future-skewed intervals are retained as history only at admission.
Reports must contain disjoint intervals, never cumulative summaries. One report can batch disjoint minutes of the same population, with at
most one sample per target/carrier/path/minute. Unknown fields, targets or enum values are refused;
there are no content, addresses, node identities or credential fields.

Latency count is 1–1,000,000, `over_target` is 0–count and max is 0–3,600,000 integer ms.
A sparse histogram contains 1–64 sorted unique `[inclusive_upper_ms,count]` pairs with
positive counts summing exactly to count. Bins are exact for 0–15 ms; above that they use
16 subdivisions per doubling, the same bucket arithmetic as daemon `Series` but applied
in integer milliseconds rather than microseconds. For `v >= 16`, let `e=floor(log2(v))`
and `step=2^(e-4)`; its bin upper bound is `(floor(v/step)+1)*step-1`.
For example 100 ms maps to 103 and 2000 maps to 2047. The last occupied bin must contain
`max_ms`. Percentiles use bin upper bounds clamped to the exact maximum. Exact over-target
count must agree with the minimum/maximum count allowed by bins that straddle the target,
and with the exact maximum. Work scales with reported buckets, never histogram counts.

Live share has no histogram, max or latency count. Foreground is 1–60,000 ms and live is
0–foreground. Foreground milliseconds form the denominator; disconnected foreground
(`foreground_ms - live_ms`) forms over-target. It aims for at least 99% live foreground time.
Neither background nor unreported/disconnected phone time is inferred.

The six latency targets in `slo/targets.toml` are `ios-open-to-live` (3000 ms),
`ios-connect` (1500), `ios-message-ack` (500), `ios-conversation-open` (1000),
`ios-terminal-open` (1500) and `ios-recover` (10000). `ios-live-share` is a separate share
entry at 99%. Client paths are `client/ios/NAME`, with carrier/path populations reported
separately from daemon response envelopes. The existing request-latency reader and
`slo/NAME` doctor lines show them; no new reader is required.

Live 1m/5m/1h windows include only whole reported intervals wholly inside the sliding UTC
window. Boundary-straddling intervals are excluded and counted; minute resolution and
`complete_coverage:false` are explicit. The 1m window may therefore have no eligible
closed minute between minute boundaries. `reported_interval_ms` counts minute slots with
reported data, not phone uptime or complete fleet coverage. Delayed reports are placed at
original observation time. Older-than-hour reports are history only. An empty population
is unknown coverage, and doctor presents it as info. These windows do not establish a
full-day SLO. Share rows contain foreground/live milliseconds and live share, never p99.

Memory is bounded to 63 target/carrier/path combinations, each with 60 minute slots and a
finite histogram vocabulary (at most 304 bins under the 3,600,000 ms limit), plus 4096
accepted IDs and at most 32 interval keys per ID. Each population-minute admits at most
one billion observations or foreground milliseconds. Intervals from different sessions
merge into the reported population. Repeating a minute from the same pairing is refused
while its accepted report is retained; identical ID/payload retries do not add a line or
sample. Conflicting ID reuse returns HTTP 409. IDs/overlap proofs expire after seven days,
on oldest-first capacity eviction or restart. Deduplication is member/process local;
retrying after eviction, restart or on a different member can count again. A buggy or
malicious paired session can also repeat a live minute after its overlap proof is evicted. Clients must
keep stable IDs and drop intervals only after local acceptance.

History is one JSONL line `{accepted_at:<unix_ms>,report:<original report>}` per newly
accepted report in `state-dir/client-observations.jsonl`. Append, rotation and expiry run
in a bounded blocking worker, outside async workers and the Store handler/SQLite writer.
Only one admission runs at a time; concurrent/busy admission returns HTTP 503 and should
retry the identical report with jitter to avoid lockstep retries. There is no outbox or retry store. Files rotate at 4 MiB or on
a UTC-day change into `.1` through `.7`, for at most eight segments / 32 MiB. Segments
older than seven days since their last append are removed on the next admission. There
is no idle cleanup thread; inactive expired files can remain until the next report.
Size rotation can reduce retained coverage; seven days is an upper retention bound,
not guaranteed coverage. Files are never replicated. Retained JSONL survives daemon
restart, but deduplication and live windows start empty and are not rebuilt from history.
Retried reports after dedup eviction or restart may also append duplicate report IDs to
history; history consumers should deduplicate by report_id. Admission mutex poisoning
fails closed with 503 until restart rather than resuming a potentially partial admission.
A torn last line is repaired from a bounded tail before append. A failed append is
truncated to its original length; any history failure returns 503 before memory admission.
Rotation can have removed old history before a subsequent append fails. No historical
content is imported as newly observed live phone time.
