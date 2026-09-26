#!/usr/bin/env bash
# JUDGE: the other two planted defects (signals, non-gating) — mergeConfig mutating its base arg, and the
# tautological mergeConfig test / missing loadConfig coverage. Reported for the reviewer; not a hard gate.
set -uo pipefail
. "$(dirname "$0")/_review-text.sh"
printf '%s' "$RL" | grep -qE 'mutat|alias|in[ -]*place|side[ -]*effect|mergeconfig.*(base|mutat)|reassign.*base' \
  && echo "  ok: review flags mergeConfig mutating its base arg" || echo "  WARN: review may MISS mergeConfig mutating its base (correctness defect #2)"
printf '%s' "$RL" | grep -qE 'assert\.ok|tautolog|weak test|trivial (assert|test)|does(n.?t| not) (really )?(test|assert|cover)|missing.*(security )?test|no.*security.*test|coverage' \
  && echo "  ok: review flags the tautological test / missing security coverage" || echo "  WARN: review may MISS the weak test / missing security coverage (defect #3)"
exit 0
