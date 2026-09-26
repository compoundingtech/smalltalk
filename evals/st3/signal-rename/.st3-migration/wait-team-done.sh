#!/usr/bin/env bash
set -euo pipefail

requester=$1
supervisor=$2
kickoff=$3
shift 3

timeout_seconds=${TIMEOUT_SECONDS:-1200}
deadline=$((SECONDS + timeout_seconds))
supervisor_actor=$supervisor
if [[ $supervisor_actor != */* ]]; then
  supervisor_actor="agent/$supervisor_actor"
fi

while (( SECONDS <= deadline )); do
  report_index=0
  if (( $# > 0 )); then
    reports=$(st3 message ls "$supervisor" --archive --json)
    all_workers_reported=true
    for worker in "$@"; do
      worker_actor=$worker
      if [[ $worker_actor != */* ]]; then
        worker_actor="agent/$worker_actor"
      fi
      index=$(jq -r --arg actor "$worker_actor" \
        '[.[] | select(.from == $actor) | .created_index] | min // 0' <<<"$reports")
      if (( index == 0 )); then
        all_workers_reported=false
      elif (( index > report_index )); then
        report_index=$index
      fi
    done
    if [[ $all_workers_reported != true ]]; then
      report_index=0
    fi
  else
    report_index=$(st3 message ls --archive --json | jq -r --arg subject "message/$kickoff" \
      '[.[] | select(.subject == $subject) | .created_index] | min // 0')
  fi

  if (( report_index > 0 )); then
    if st3 message ls "$requester" --archive --json | jq -e \
      --arg actor "$supervisor_actor" --argjson report "$report_index" \
      'any(.[]; .from == $actor and .created_index >= $report)' >/dev/null; then
      exit 0
    fi
  fi
  sleep 1
done

echo "The team did not report completion before the timeout." >&2
exit 1
