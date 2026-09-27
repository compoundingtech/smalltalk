# Recover a stuck run

This example starts after an invented archive import has stopped making progress. Its exact run is
`mission-run/example/archive-import/first`.

## Failure first: cancellation enters final work

Cancel the run and inspect it:

```sh
st3 missions cancel mission-run/example/archive-import/first \
  --reason 'The input archive was withdrawn.' \
  --as person/operator
st3 missions show mission-run/example/archive-import/first
```

Cancellation revokes ordinary work and enters the mission's `finally` graph. It does not declare
the run terminal before that final work settles. In this failure, `missions show` reports phase
`final-cancelled`, and the run's own `close-ledger` final step remains pending and unscheduled.

The next tempting command appears to say that repair cannot help:

```sh
st3 repair dry-run
```

If less than 60 seconds have elapsed since the run last changed, a `clean` result is expected. The
repair planner deliberately has a one-minute safety window so it does not race ordinary
reconciliation. `clean` during that window does not prove that the cancelled final phase can
recover by itself.

## Supported recovery: inspect, wait for the fence, then apply one plan

First confirm the exact shape rather than repairing an unrelated run:

```sh
st3 missions show mission-run/example/archive-import/first
st3 work ls --all
```

If final work is ready, claimed, working, or delayed until a future time, let that normal lifecycle
finish. The bounded repair below is for the narrower case where every remaining final step is only
pending and unscheduled.

Once the run has remained in that shape for a full minute, preview again:

```sh
st3 repair dry-run
```

The plan should name class `cancelled-final-stall`, the exact mission run, its pending final steps,
and an approval token. Check all of those fields. Then apply only that exact token:

```sh
st3 repair apply orpv0:PREVIEW_TOKEN
st3 missions show mission-run/example/archive-import/first
st3 repair dry-run
```

The repaired run is terminal and cancelled, its remaining final work is cancelled, and a second
dry-run is clean. If the preview names a different repair class or subject, do not apply it as a
substitute for diagnosis.
