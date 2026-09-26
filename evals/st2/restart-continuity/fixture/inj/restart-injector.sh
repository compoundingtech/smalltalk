#!/usr/bin/env bash
set -euo pipefail

ledger="$CATALOG/worker"
bus="${ST_ROOT:-$CATALOG}"
state="$CATALOG/.stev"
stamp="$state/restart.done"
log="$state/restart.log"
trace="$state/injector.trace"
mkdir -p "$state"

item_commits() {
  local base
  base="$(git -C "$ledger" rev-list --max-parents=0 HEAD 2>/dev/null || true)"
  if [ -z "$base" ]; then
    printf '0\n'
    return
  fi
  git -C "$ledger" rev-list --count "$base..HEAD" 2>/dev/null || printf '0\n'
}

worker_route() {
  local route
  route="$(find "$bus" -mindepth 1 -maxdepth 1 -type d \( -name 'rc.dev' -o -name '*.rc.dev' \) -print -quit 2>/dev/null)"
  printf '%s\n' "${route:-$bus/rc.dev}"
}

if [ ! -f "$stamp" ]; then
  {
    printf 'catalog=%s\n' "$CATALOG"
    printf 'ledger=%s\n' "$ledger"
    printf 'head_log=%s\n' "$ledger/.git/logs/HEAD"
  } >"$trace"

  previous_count=-1
  while :; do
    current_count="$(item_commits)"
    if [ "$current_count" -ne "$previous_count" ]; then
      printf 'item_commits=%s epoch=%s\n' "$current_count" "$(date +%s)" >>"$trace"
      previous_count="$current_count"
    fi
    [ "$current_count" -ge 2 ] && break
    sleep 0.25
  done

  route="$(worker_route)"
  messages="$route/resources"
  printf 'message_root=%s\n' "$messages" >>"$trace"
  original=""
  while [ -z "$original" ]; do
    original="$(grep -lRE '^from:[[:space:]]*([a-z0-9][a-z0-9._-]*[.])?rc[.]sup([[:space:]]|$)' "$messages/archive" "$messages/inbox" 2>/dev/null | head -1 || true)"
    [ -n "$original" ] || sleep 0.25
  done

  pre_head="$(git -C "$ledger" rev-parse HEAD)"
  message_name="$(date +%s%3N)-duplicate-rc.md"
  mkdir -p "$messages/inbox"
  cp -- "$original" "$messages/inbox/$message_name"

  {
    printf 'restart_epoch=%s\n' "$(date +%s)"
    printf 'pre_restart_head=%s\n' "$pre_head"
    printf 'item_commits_at_restart=%s\n' "$current_count"
    printf 'duplicate_message=%s\n' "$message_name"
    printf 'action=cold_restart\n'
  } >"$log"
  touch "$stamp"

  worker_id="$(basename "$route")"
  printf 'worker_id=%s\n' "$worker_id" >>"$trace"
  st2 pty kill "$worker_id"
  printf 'restart_requested=true\n' >>"$log"
fi

while :; do sleep 3600; done
