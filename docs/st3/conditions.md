# Declared conditions

A condition is a root KDL fleet object. Each daemon evaluates the instances on
its own host every 30 seconds, using local measurements. Owners receive one
notification per instance on entering breach and one on recovery. Samples and intermediate
hold phases do not wake an owner.

```kdl
version 2
condition "fleet/disk" {
  metric "disk.free-percent"
  scope "host"
  below 15
  recover 18
  for "10m"
  recover-for "5m"
  owner "agent/operations"
}
```

This checks up to eight local data filesystems on every host. An overflow is reported in doctor; use explicit path declarations for additional data mounts. Add `host "alder"` to
restrict evaluation to that member, or several `host` children for several
members. Each matching host evaluates and notifies independently. Add `path "/srv/data"` to check the filesystem containing one absolute
path. A process condition needs `process "collector"`; a route condition needs
`route "person-read"` (a target name from `slo/targets.toml`) or a measured path.
Route and daemon CPU metrics accept `window "1m"`, `"5m"`, or `"1h"`. Routes default to `"1h"`; daemon CPU defaults to `"5m"`.

| Scope | Metrics | Units |
| --- | --- | --- |
| host | `disk.free-percent`, `memory.available-percent` | percent |
| host | `disk.free-bytes` | bytes |
| process | `process.cpu-cores` | CPU seconds per elapsed second, summed by process name |
| process | `process.rss-bytes` | resident bytes, summed by process name |
| member | `db.size-bytes` | database and WAL bytes |
| member | `db.authored-bytes-per-day` | locally authored claim bytes per day |
| member | `cost.usd-per-day` | recorded model spend in USD over 24 hours |
| member | `daemon.cpu-cores` | daemon CPU from SLO windows |
| route | `slo.burn-rate` | share over target divided by the 1% p99 error budget |
| route | `slo.p99-ms` | milliseconds |

Use exactly one of `above NUMBER` and `below NUMBER`. Crossing is strict;
equality does not enter breach. `recover NUMBER` supplies hysteresis, defaulting
to the breach threshold. `for` is required and is the continuous hold before
entry. Both holds must be at least 60 seconds. `recover-for` defaults to the entry hold.
After recovery an instance waits five minutes before starting another entry hold. A missing reading interrupts a
hold and keeps an established breach. A sampling gap over 75 seconds or a clock
reversal also interrupts holds; machine sleep does not count. After a daemon restart, holds begin again
and an established breach continues without sending another entry message.

Disk, process and available-memory probes use kernel facts on Linux. macOS reads
cached native mount facts with `getfsstat`; process and memory readings can be
unavailable. Disk selectors must name absolute paths on native filesystems.
Network, autofs, FUSE and symlink selectors are unavailable. One disk worker
reads at most 256 native mounts and refreshes a cache every 30 seconds. A kernel
call taking more than ten seconds produces a diagnostic; disk readings expire
after 90 seconds while other metrics continue. The worker waits for that call
before starting another. Filesystem IDs deduplicate bind mounts where the
kernel reports the same ID; btrfs subvolumes may have distinct IDs.
A process tick visits at most 16,384 entries. Names match `/proc/PID/comm`
exactly, are at most 15 bytes, and CPU needs two samples.
Database size measures actual database file plus WAL lengths once per evaluation,
never on a request path; it is live at the 30-second cadence. Physical daily growth
is deferred until the shared hourly SLO sampler supplies file+WAL history; no second
ring is created here. The first growth declaration follows that adapter and fleet
deployment, using operations' last-week physical-growth baseline. The separate
authored-byte metric attributes each member's
contribution to the replicated claim log in hourly buckets over 24 hours; a count
still catching up has no reading.

`owner` accepts `agent/NAME` or `person/NAME`. Agent notifications cite the state
claim and include the value, threshold, breach start, and series location.
Person breaches appear on their home and clear after recovery. Optional
`link "https://metrics.example.com/series?host={host}"` points to an external
series; `{host}` and `{instance}` are substituted. The default is the local
`st conditions show` command.

Read declarations, instance states and their recent samples with:

```sh
st conditions ls
st conditions show fleet/disk
st conditions ls --json
st doctor
```

These views read cached values and recorded transitions. Kernel probes, folds
and notification retries run in the daemon background. Routine samples and
intermediate hold phases never append claims or wake the reconciler. Each
local instance keeps its eight most recent exact samples; notification values
are formatted to three significant digits. Only entry and recovery append
`condition.state`, on
`condition-instance/SHA256(condition-root)/SHA256(origin)/SHA256(instance-name)` with the
condition root in its fields. Latest-state startup seeks skip historical
claims using the existing subject index. Remote views show the last transition;
only the evaluating daemon has fresh routine values.

The daemon accepts at most 32 declarations, 32 host selectors per declaration,
and eight local instances per condition. Its transition budget is 16 attempted
writes per tick, with deferred transitions retained for later ticks. Local
heads retain eight instances per authenticated origin and at most 256 total per declaration. Startup skips the remainder of each origin namespace so excess instances cannot hide another host. A changed declaration set re-seeds indexed state, including transitions received before declarations. Rebuild discovery considers at most 64 instance namespaces per origin, keeping eight newest candidates and retaining newer cached heads; it does not scan their history. Names, selectors,
links, notification text and sample rings have fixed size limits. Retired
conditions lose their local caches. Vanished instances leave the active tracker
set; an established breach remains as a stale last-known breach within the bounded instance budget, since absence
cannot prove recovery. Current local observations and newer transition heads have priority when retired mounts exhaust that budget. A returning instance restores that breach silently.

`st doctor` checks the local evaluator heartbeat and sample age. Breaches,
recovering states, invalid declarations and stale data warn; a pending hold or
an instance awaiting its first reading is informational. Other members' breaches
do not fail the local strict doctor check. Person attention labels stale local breach measurements; remote values describe the last transition and carry no freshness assertion. Stale Clear observations do not warn. A panicking tick is caught, reported and retried.

The durable notification queue retries recorded transitions across restarts;
deterministic message identities prevent duplicate wakeups. Startup only queues
own-host transitions accepted in the last hour. A failing notification receives
three data-validation attempts, then leaves the queue with a diagnostic visible in doctor, so
it cannot block other owners. Writer contention keeps the row without consuming attempts; a clean flush clears the transient diagnostic. At most four notifications per owner and 16 total are attempted per tick. Authenticated claim origin and instance identity
must agree with the host fields; received state cannot send as this member.

The per-instance state subject and the condition root field are the seam for
future waits on a breach. This feature does not implement those waits or accept
OpenObserve alert webhooks. Upgrade every fleet daemon before publishing
conditions; daemon before CLI, and upgrade clients before a person-owned
condition is declared. An older registry's unknown-subject-family rejection is
retried once when its schema digest changes; genuinely invalid subjects remain
invalid. Operations publishes declarations after confirming every member runs
the new schema. Existing disk and watchdog missions remain until a real host
has fired and recovered and operations agrees to retire them.
