#!/usr/bin/env bash
# JUDGE: coordination — the delegate->report loop is visible on the bus: a gbx.sup -> gbx.fix delegation and a
# gbx.fix -> gbx.sup report. A completed fix with no worker->sup report is the signature of out-of-band /
# sup-did-it-itself work. (The run ends on the sup's confirmation to the requester; this judge reproduces
# the held-out grader's coordination gate, which is delegate + report.)
#
# PASS (exit 0): both the delegation and the report are present on the bus.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"
SM="${ST_ROOT:?st2 eval must export ST_ROOT}"
SUP_ID="${SUP_ID:-gbx.sup}"; WORKER_ID="${WORKER_ID:-gbx.fix}"

# Resolve an id to its on-disk bus dir, tolerating a leading team/host prefix.
busdir(){ local id="$1" d; d="$(ls -d "$SM"/*."$id" "$SM/$id" 2>/dev/null | head -1)"
  d="${d:-$SM/$id}"; [ -d "$d/resources/inbox" ] && d="$d/resources"; printf '%s\n' "$d"; }
msgs_from(){ local owner from; owner="$(busdir "$1")"; from="$2"
  grep -lRE "^from:[[:space:]]*([a-z0-9][a-z0-9._-]*\.)?$from([[:space:]]|\$)" "$owner/inbox" "$owner/archive" 2>/dev/null; }

fail=0
deleg=$(msgs_from "$WORKER_ID" "$SUP_ID")   # sup -> worker (lands in the worker's box)
report=$(msgs_from "$SUP_ID" "$WORKER_ID")  # worker -> sup (lands in the sup's box)
[ -n "$deleg" ]  && echo "PASS: sup -> worker delegation present on the bus" \
                 || { echo "FAIL: no sup -> worker delegation on the bus (delegation not visible)"; fail=1; }
[ -n "$report" ] && echo "PASS: worker -> sup report present on the bus" \
                 || { echo "FAIL: no worker -> sup report on the bus (execute/report not visible)"; fail=1; }
exit "$fail"
