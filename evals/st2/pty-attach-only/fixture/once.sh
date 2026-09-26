#!/usr/bin/env bash
set -euo pipefail

printf 'run\n' >>"${1:?marker path required}"
exit 42
