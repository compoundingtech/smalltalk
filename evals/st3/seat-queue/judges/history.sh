#!/usr/bin/env bash
# Shared by the held-out judges: the complete graph history, oldest first, one
# claim per line.

dump_history() {
  local output=$1 after=0 page last
  : >"$output"
  while :; do
    page="$(st3 trace show --json --after-index "$after" --limit 500)"
    [[ -n "$page" ]] || break
    printf '%s\n' "$page" >>"$output"
    last="$(jq -s 'map(.store_index) | max' <<<"$page")"
    (( last > after )) || break
    after=$last
  done
  test -s "$output"
}
