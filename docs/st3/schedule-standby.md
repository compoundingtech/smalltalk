# Automatic schedule standby

A schedule may name one standby member. Without `standby`, schedules keep their
existing primary-only behavior.

```kdl
schedule "orchard-hourly" {
  host "laptop"
  standby {
    host "archive"
    after "5m"
  }
  every "1h"
  anchor "2030-01-01T00:00:00Z"
  catch-up "latest"
  work { mission "orchard"; workspace "/srv/orchard" }
}
```

**Jobs must be safe to run twice.** While hosts cannot hear each other a job may
run on both. Write jobs so a second run is harmless. Both starts use the same
occurrence identity and their records converge when replication resumes; this
cannot undo an external effect performed twice. There is no exclusive lease.

`standby` has exactly one `host` and one `after` child. `after` is required, must
be a positive finite duration, and has no default. The standby must be an explicit
member name distinct from the primary. A second standby, duplicate/unknown
setting, properties, or `host "local"` in the standby block is refused. The
existing omitted-primary-host and `host "local"` rules are unchanged.

With known membership, publishing an unknown or removed standby member is
refused. If membership is not yet known, a parsed declaration cannot enable
standby; show reports **standby member unknown, never eligible**. Every active
member must advertise `schedule_standby=1` before enabling the feature. Missing
advertisements, old members, conflicting membership, or unfenced legacy writers
hold standby admission. Mixed fleets remain primary-only. Upgrade all members
before publishing a standby declaration; activation is roll-forward only.

## Heartbeat and restart behavior

The heartbeat is this standby's local observation of an authenticated replication
exchange with the primary, in either direction. Another member's observation of
the primary is not a heartbeat. Age uses monotonic elapsed time, conservatively
starting at zero when observed. An age equal to `after` is still fresh; takeover
requires an older observation. Wall-clock jumps and peer clock skew cannot supply
elapsed monotonic time. A newer exchange refreshes the observation. On a standby host that suspends,
monotonic time may stop while wall time advances. Resume then drops the sample
and restarts the wait conservatively; suspended time never proves silence.

The standby needs a complete applied primary exchange in its current daemon
incarnation and the schedule's durable occurrence history. Unknown, missing,
unreadable, incomplete or unprojected state cannot prove an occurrence absent.
The local view can still miss a primary start made during a partition. The
run-twice rule applies to that window, including while the primary returns and
replication has not delivered the standby's newest start.

If the standby restarts while the primary is down, the standby cannot take over until it has heard the primary once; a run in that window may be skipped (catch-up policy applies when the primary returns).

This first implementation preserves the existing cold-admission guard. A stored
last-success timestamp, peer inventory digest or historical first-sync result
does not establish complete applied primary coverage after restart.

When the primary returns, the standby stops admitting new work. Existing work
finishes where it started; there is no manual takeover or mid-run handback. The
primary's cold/catch-up guard consumes available reached/run facts before
admitting work. If it knows an occurrence already ran, it does not replay it.

## Catch-up, missing work, and inspection

Existing catch-up policies apply: `latest` starts the current missed occurrence
once; `skip` moves on to the next occurrence; `all` holds if missed work exceeds
`max-catch-up`. No standby wake reopens an already-run occurrence or bypasses
these bounds after a long outage. One-shot and calendar occurrence identities,
including their recorded UTC instants, are unchanged.

The mission and workspace must be usable on the standby; naming a standby does
not rewrite mission placement. An unavailable mission/revision, owner or workspace
records one typed `standby-work-unavailable` failure for that occurrence rather
than retrying forever. Later occurrences may run after the underlying issue is
fixed, subject to their normal catch-up policy.

`st subject show schedule/NAME` shows the standby declaration, required age,
this member's observation and reason, and both warnings. The structured response
adds optional `schedule_standby`; the underlying exact status read carries it in
`actual.schedule_standby`. It is local observation, not a fleet-wide lease or an
assurance that an occurrence will start.

`st subject history schedule/NAME` prints the same warnings. Structured history
rows have the optional observation field. Takeovers add optional `standby` detail
and a plain reason to existing occurrence scheduled/reached claims: host, primary,
observed heartbeat age and known local view. Original claims remain visible;
convergence never erases evidence that both hosts performed work.
