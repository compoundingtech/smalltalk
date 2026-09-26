#!/usr/bin/env bash
set -euo pipefail

ledger="$CATALOG/worker"
state="$CATALOG/.stev"
stamp="$state/restart.done"
log="$state/restart.log"
subject="agent/${ST_MISSION_RUN}/rc.dev"
mkdir -p "$state"

actual() {
  st3 subject show "$subject" --json | jq -c '.status.subjects[0].actual | .fields // .'
}

old_actual="$(actual)"
old_incarnation="$(jq -r '.incarnation_id // empty' <<<"$old_actual")"
[ -n "$old_incarnation" ]
pre_head="$(git -C "$ledger" rev-parse HEAD)"

st3 terminals signal "$subject" hangup >/dev/null

new_incarnation=""
for _ in $(seq 1 1200); do
  current="$(actual 2>/dev/null || true)"
  incarnation="$(jq -r '.incarnation_id // empty' <<<"${current:-{}}" 2>/dev/null || true)"
  harness_state="$(jq -r '.state // empty' <<<"${current:-{}}" 2>/dev/null || true)"
  if [ -n "$incarnation" ] && [ "$incarnation" != "$old_incarnation" ]; then
    case "$harness_state" in
      ready|idle|working)
        new_incarnation="$incarnation"
        break
        ;;
    esac
  fi
  sleep 0.25
done
[ -n "$new_incarnation" ]

duplicate_id="$(st3 conversations send "$subject" \
  --from "$ST3_SUBJECT" \
  --subject "Repeated pre-restart work" \
  --tags "mission-run:$ST_MISSION_RUN,duplicate-work:process-before-restart" \
  -m "DUPLICATE-BATCH-RC-7B9D: This repeats work assigned before the cold restart. Read the durable st3 missions, PROGRESS.md, and git history. Do not redo items 1 or 2. Continue only ready assigned work.")"

st3 claim "resource/mission-run/$ST_MISSION_RUN/restart" resource.observed \
  --actor "$ST3_SUBJECT" \
  --field kind=custom.st3.cold-restart \
  --field state=injected \
  --field old_incarnation="$old_incarnation" \
  --field new_incarnation="$new_incarnation" \
  --field duplicate_message="message/$duplicate_id" >/dev/null

{
  printf 'restart_epoch=%s\n' "$(date +%s)"
  printf 'pre_restart_head=%s\n' "$pre_head"
  printf 'old_incarnation=%s\n' "$old_incarnation"
  printf 'new_incarnation=%s\n' "$new_incarnation"
  printf 'duplicate_message=message/%s\n' "$duplicate_id"
  printf 'action=cold_restart\n'
} >"$log"
touch "$stamp"
