#!/usr/bin/env bash
set -euo pipefail

: "${MESSAGE:?The text input is required.}"
: "${SOURCE:?The resource input is required.}"
printf '%s' "$MESSAGE" >captured-message.txt
printf '%s\n' "$SOURCE" >captured-source.txt
