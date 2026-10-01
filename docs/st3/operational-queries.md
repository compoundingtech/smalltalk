# Operational queries

Mission lists page mission IDs and return compact cards with at most three recent runs and
twenty step previews per run. Progress counts cover every step; a larger total signals an
incomplete step preview.
`active_runs`, `total_runs`, and `run_counts` count the entire history. `runs_truncated` says when
the preview is incomplete. A page also respects a byte budget; its cursor resumes after the last
returned mission. Full step details remain available by exact run subject.

```sh
st missions ls --all
st missions ls --all --cursor 'CURSOR'
st missions show mission/example/build
```

Showing a mission with multiple runs prints counts by state, the ten newest runs, and the ten
newest failed runs with reasons. Showing a mission with one run opens that run. `--follow`
requires choosing an exact run when the mission has multiple runs.

To investigate outcomes in a time window:

```sh
st missions ls --since 6h --status failed
st work ls --since 6h --status timed-out
st work ls --since 2026-10-01T00:00:00Z --until 2026-10-01T06:00:00Z --status cancelled
```

`--since` and `--until` accept RFC3339 timestamps or durations in milliseconds, seconds,
minutes, or hours. Supplying either time filter or `--status` selects terminal transition history
across missions. Status accepts `failed`, `cancelled`, `timed-out`, and `completed`; without a
status filter, all terminal transitions are returned. A timeout is a failed transition whose
recorded reason names a timeout. Cleanup sometimes omits the reason in the terminal claim;
the reader recovers it from an earlier transition for that outcome.

These are events, so retries preserve earlier failures and a run can appear more than once.
Times are the graph claims' accepted timestamps. `--as` on work also filters by the recorded
actor or step assignment. Pages contain reasons and their claim IDs; the continuation printed
by the command fixes the original time window. Resume using that command without repeating
relative time filters. JSON includes `next_cursor`, and an exhausted page has `has_more=false`.

```sh
st doctor
st doctor --performance
st doctor --performance --json
```

The daemon always keeps bounded request, background task, and SQL timing aggregates for the
last five minutes, in ten-second buckets. Doctor reports the twenty request/task kinds and
twenty query templates with the most total wall time, including counts, mean time, maximum
time, and recorded thread CPU for requests/tasks. Request CPU includes store work dispatched
to other blocking workers. SQL literals and comments are removed before aggregation. SQLite
statement wall time includes row processing, and concurrent statement times overlap.

This accounting writes no graph claims and requires no file profiling configuration. Samples
are local to the daemon and disappear on restart. `--performance` requests the timing report
without running dependency checks. File profiling remains available for deeper investigation.

The local API serves `/v1/mission-overview?mission=...`, `/v1/outcome-history?collection=...`,
and `/v1/performance`. Outcome API times are epoch milliseconds; `before` resumes by immutable
claim index and `limit` is between 1 and 200.
