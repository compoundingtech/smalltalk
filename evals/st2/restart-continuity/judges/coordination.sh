#!/usr/bin/env bash
set -euo pipefail

root="${CATALOG:-$PWD}"
bus="${ST_ROOT:-$root}"

bus_dir() {
  local identity="$1"
  local directory
  directory="$(find "$bus" -mindepth 1 -maxdepth 1 -type d \
    \( -name "$identity" -o -name "*.$identity" \) -print -quit 2>/dev/null || true)"
  directory="${directory:-$bus/$identity}"
  test -d "$directory/resources/inbox" && directory="$directory/resources"
  printf '%s\n' "$directory"
}

messages_from() {
  local owner="$1"
  local sender="$2"
  local directory
  local sender_pattern="${sender//./[.]}"
  directory="$(bus_dir "$owner")"
  grep -lRE "^from:[[:space:]]*([a-z0-9][a-z0-9._-]*[.])?$sender_pattern([[:space:]]|$)" \
    "$directory/inbox" "$directory/archive" 2>/dev/null || true
}

line_count() {
  if test -z "$1"; then printf '0\n'; else printf '%s\n' "$1" | grep -c .; fi
}

message_time() {
  basename "$1" | grep -oE '^[0-9]+'
}

delegations="$(messages_from rc.dev rc.sup)"
reports="$(messages_from rc.sup rc.dev)"
confirmations="$(messages_from requester rc.sup)"
kickoffs="$(messages_from rc.sup requester)"

test "$(line_count "$delegations")" -eq 2
test "$(line_count "$reports")" -eq 1
test "$(line_count "$confirmations")" -eq 1
test "$(line_count "$kickoffs")" -eq 1

unique_delegations="$(while read -r file; do sha256sum "$file"; done <<<"$delegations" | cut -d' ' -f1 | sort -u | wc -l)"
test "$unique_delegations" -eq 1

duplicate="$(sed -n 's/^duplicate_message=//p' "$root/.stev/restart.log" | tail -1)"
worker_dir="$(bus_dir rc.dev)"
test -f "$worker_dir/inbox/$duplicate" || test -f "$worker_dir/archive/$duplicate"

report_time="$(message_time "$reports")"
confirmation_time="$(message_time "$confirmations")"
test "$confirmation_time" -gt "$report_time"

echo "PASS: Small Talk has one kickoff, repeated delegation, one report, and one later confirmation"
