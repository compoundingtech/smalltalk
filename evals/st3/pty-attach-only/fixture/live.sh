#!/usr/bin/env bash
set -euo pipefail

printf 'run\n' >>"${1:?marker path required}"
printf 'ATTACH-ONLY-LIVE-READY\n'
IFS= read -r line
printf 'LIVE-ACK:%s\n' "$line"
exit 37
