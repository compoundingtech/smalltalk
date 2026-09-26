#!/usr/bin/env bash
set -euo pipefail

sleep 15
sender="$(basename "$(find "$CATALOG" -maxdepth 1 -type d -name '*ce.reporter' | head -1)")"
st2 message send ce.sup --as "$sender" --subject "Probe complete" -m REPORTER-GREEN
sleep 300
