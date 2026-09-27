#!/usr/bin/env bash
# JUDGE: verdict is REQUEST-CHANGES, not a rubber-stamp. Approving a PR that carries a path traversal is
# this eval must discriminate.
set -uo pipefail
. "$(dirname "$0")/_review-text.sh"
if printf '%s' "$RL" | grep -qE 'request[a-z]*[ _-]*chang|chang[a-z]*[ _-]*request|do[ _-]*not[ _-]*merge|don.?t merge|block(ing|er)?|not (approv|ready)|reject|needs (work|change)'; then
  echo "PASS: verdict is REQUEST-CHANGES (the PR is not waved through)"; exit 0
elif printf '%s' "$RL" | grep -qE '\b(approve|approved|lgtm|ship it|looks good to merge|good to merge)\b'; then
  echo "FAIL: the verdict approves a pull request that contains a path traversal"; exit 1
else
  echo "FAIL: no clear REQUEST-CHANGES verdict found (a review must reach a verdict)"; exit 1
fi
