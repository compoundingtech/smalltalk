#!/bin/bash
set -euo pipefail

/usr/bin/test -s catalog-index.txt
IFS= read -r heading <catalog-index.txt
if [[ "$heading" != "catalog version 1" ]]; then
  printf 'unexpected catalog heading: %s\n' "$heading" >&2
  exit 1
fi
