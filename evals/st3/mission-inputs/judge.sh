#!/usr/bin/env bash
set -euo pipefail

: "${EXPECTED_MESSAGE:?The expected text input is required.}"
: "${EXPECTED_SOURCE:?The expected resource input is required.}"
test "$(cat captured-message.txt)" = "$EXPECTED_MESSAGE"
test "$(cat captured-source.txt)" = "$EXPECTED_SOURCE"

claim_id="${EXPECTED_SOURCE##*@}"
st3 --json inspect "$EXPECTED_SOURCE" |
  jq -e \
    --arg reference "$EXPECTED_SOURCE" \
    --arg claim_id "$claim_id" \
    '.reference == $reference and .claim.id == $claim_id and .claim.subject == "resource/mission-inputs/source" and .claim.kind == "resource.observed"' \
    >/dev/null
