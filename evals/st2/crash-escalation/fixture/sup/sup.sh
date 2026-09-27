#!/usr/bin/env bash
set -euo pipefail

for _ in $(seq 1 100); do
  route="$(find "$CATALOG" -maxdepth 1 -type d -name '*ce.sup' | head -1)"
  crash="$(grep -rl 'worker crash: .*ce.crash' "$route/inbox" "$route/archive" 2>/dev/null || true)"
  report="$(grep -rl 'REPORTER-GREEN' "$route/inbox" "$route/archive" 2>/dev/null || true)"
  if [ -n "$crash" ] && [ -n "$report" ]; then
    st2 message send requester --as "$(basename "$route")" --subject "Crash contract complete" -m "The abnormal exit was reported and the clean control finished."
    sleep 300
  fi
  sleep 0.2
done
exit 1
