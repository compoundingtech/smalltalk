#!/usr/bin/env bash
set -euo pipefail

printf '\033[31mINITIAL_COLOR_61e8\033[0m\r\n'
while IFS= read -r line; do
  case "$line" in
    EXIT_61e8) printf 'FINAL_DATA_61e8\r\n'; exit 0 ;;
    *) printf 'LIVE:%s\r\n' "$line" ;;
  esac
done
