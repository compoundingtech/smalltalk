#!/usr/bin/env bash
set -Eeuo pipefail

grep -Fxq PLANNING-MODE-GREEN result.txt
test -s approved-preview-hash.txt
test -s approved-mission.md
test -s approved-mission.kdl
jq -e '
  .status == "approved"
  and .preview.hash == $hash
  and .published_revision == .candidate.mission_revision
' --arg hash "$(cat approved-preview-hash.txt)" approved.json >/dev/null
jq -e '
  ([.recent_claims[] | select(.kind == "mission.published")] | length) == 1
' mission-inspect.json >/dev/null
jq -e '
  ([.recent_claims[] | select(.kind == "planning-session.approved")] | length) == 1
' planning-session-inspect.json >/dev/null
