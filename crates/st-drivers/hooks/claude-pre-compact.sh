#!/usr/bin/env bash
# The persistence behavior is harness-agnostic. Keep this entrypoint trivial and fail-open so a
# parse/runtime error in the implementation can never block Claude compaction.

impl="$(dirname "$0")/codex-pre-compact.sh"
if [[ -r "$impl" ]]; then
  bash "$impl" "$@" || true
fi
exit 0
