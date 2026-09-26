#!/usr/bin/env bash
set -euo pipefail

: "${ST3_ENDPOINT:?ST3_ENDPOINT must identify the eval daemon}"
: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the root mission run}"

deadline=$((SECONDS + 600))
subscription="subscription/${ST_MISSION_RUN}/reviews"
review_run=""

while ((SECONDS < deadline)); do
  if [[ -z "$review_run" ]]; then
    subscription_json="$(
      st3 --endpoint "$ST3_ENDPOINT" inspect "$subscription" --json
    )"
    review_run="$(
      jq -r \
        '[.recent_claims[] | select(.kind == "subscription.mission-started") | .body.fields.mission_run] | last // empty' \
        <<<"$subscription_json"
    )"
  fi

  if [[ -n "$review_run" ]]; then
    run_json="$(st3 --endpoint "$ST3_ENDPOINT" inspect "$review_run" --json)"
    run_status="$(jq -r '.status.subjects[0].actual.status // empty' <<<"$run_json")"
    run_phase="$(jq -r '.status.subjects[0].actual.phase // empty' <<<"$run_json")"

    if [[ "$run_phase" == "terminal" && "$run_status" != "completed" ]]; then
      printf '%s reached terminal status %s\n' "$review_run" "$run_status" >&2
      exit 1
    fi

    if [[ "$run_phase" == "terminal" && "$run_status" == "completed" ]]; then
      mapfile -d '' reviews < <(
        find reviews -name REVIEW.md -type f -size +0c -print0 2>/dev/null
      )
      if ((${#reviews[@]} == 1)); then
        exit 0
      fi
      printf 'expected one completed review, found %s\n' "${#reviews[@]}" >&2
      exit 1
    fi
  fi

  sleep 2
done

printf 'no review mission completed before the deadline\n' >&2
exit 1
