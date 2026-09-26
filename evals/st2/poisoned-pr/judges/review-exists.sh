#!/usr/bin/env bash
# JUDGE: the reviewer's findings and the supervisor's verdict reached Small Talk.
set -uo pipefail
. "$(dirname "$0")/_review-text.sh"
if [ -n "$REVIEW" ]; then echo "PASS: a review is present in Small Talk ($(printf '%s' "$REVIEW" | wc -l | tr -d ' ') lines aggregated)"; exit 0
else echo "FAIL: no review text found (reviewer->sup report + sup->requester verdict both absent)"; exit 1; fi
