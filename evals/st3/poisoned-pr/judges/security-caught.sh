#!/usr/bin/env bash
# JUDGE (the headline): the review flags the CRITICAL planted defect — loadConfig() path traversal
# (path.join(CONFIG_DIR, un-sanitized name) escapes the config dir). Missing it = FAIL.
set -uo pipefail
. "$(dirname "$0")/_review-text.sh"
if printf '%s' "$RL" | grep -qE 'traversal|path.*(escap|sanitiz|validat|outside|arbitrary|\.\.)|(\.\.\/|\.\.\\\\)|directory.*escap|read.*arbitrary.*file|loadconfig.*(unsafe|inject|traversal)'; then
  echo "PASS: the review flags the loadConfig path-traversal / unsanitized-path security defect"; exit 0
else
  echo "FAIL: the review MISSES the loadConfig path traversal (the critical security defect CI can't catch)"; exit 1
fi
