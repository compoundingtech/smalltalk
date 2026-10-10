# Declared conditions

A condition is a root KDL fleet object. Each daemon evaluates the instances on
its own host every 30 seconds, using local measurements. Owners receive one
notification on entering breach and one on recovery. Samples and intermediate
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

This checks every local data filesystem on every host. Add `host "alder"` to
restrict evaluation to that member, or several `host` children for several
members. Add `path "/srv/data"` to check the filesystem containing one absolute
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
| member | `db.growth-bytes-per-day` | locally authored claim bytes per day |
| member | `cost.usd-per-day` | recorded model spend in USD over 24 hours |
| member | `daemon.cpu-cores` | daemon CPU from SLO windows |
| route | `slo.burn-rate` | share over target divided by the 1% p99 error budget |
| route | `slo.p99-ms` | milliseconds |

Use exactly one of `above NUMBER` and `below NUMBER`. Crossing is strict;
equality does not enter breach. `recover NUMBER` supplies hysteresis, defaulting
to the breach threshold. `for` is required and is the continuous hold before
entry. `recover-for` defaults to the entry hold. A missing reading interrupts a
hold and keeps an established breach. A sampling gap over 75 seconds or a clock
reversal also interrupts holds; machine sleep does not count. After a daemon restart, holds begin again
and an established breach continues without sending another entry message.

Disk, process and available-memory probes use kernel facts on Linux. macOS reads cached mount facts with `getfsstat`; other hosts
check `/` when no mount table is available; process and memory readings can be
unavailable. A tick reads at most 256 local filesystems and 16,384 process entries. Process names match `/proc/PID/comm` exactly. CPU needs two samples.
Database growth counts this member's contribution to the replicated claim log,
rather than physical file allocation, compaction or another member's claims.
It uses hourly buckets over 24 hours; a count still catching up has no reading.

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

These views read recorded facts. Kernel probes, state folds and notification
retries run in the daemon background. Steady samples do not append claims;
changed values and intermediate phases are recorded at most every five minutes,
and transitions are recorded immediately. Each state carries eight samples.
The durable notification queue retries recorded transitions across restarts;
deterministic message identities prevent duplicate wakeups.

The `condition/NAME` subject and its `condition.state` claims are the seam for
future waits on a breach. This feature does not implement those waits or accept
OpenObserve alert webhooks. Upgrade every fleet daemon before publishing the
new declaration kind.
