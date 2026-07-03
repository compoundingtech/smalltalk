#!/bin/bash
# coord session-start nudge — wakes Claude with a system reminder to
# run the coord boot ritual on every cold start, --resume, --continue,
# /resume, /clear, and /compact.
#
# brief-024 hook-legs: this script now ALSO injects the agent's last
# durable working-state (context/now.md) as a <context> block so a
# restarted agent picks the task back up rather than reconstructing
# from stale files. Absent-able: if context/now.md is missing or has
# aged past the staleness threshold (default 24h), we skip the
# injection and emit only the boot-ritual reminder — same as before
# this hook-leg landed.
#
# Install per coord/examples/claude-code/README.md.
#
# The existing boot-ritual mechanism: exit code 2 + stderr is what
# Claude Code treats as a system reminder under `asyncRewake: true`,
# forcing a turn so the reminder gets acted on.

set -uo pipefail

# ─── Identity + root resolution (identical to pre-compact.sh) ────────────

identity="${COORD_IDENTITY:-${ST_IDENTITY:-${ST_AGENT:-}}}"
# `${HOME-}` (not `${HOME:-}`) so we don't trip `set -u` if HOME is
# somehow unset in the hook's env — reduces to empty in that path and
# any coord_root use is guarded by an `-n "$identity"` check below.
coord_root="${COORD_ROOT:-${ST_ROOT:-${HOME-}/.local/state/coord}}"

# The staleness threshold, in seconds. now.md older than this is
# NOT injected — stale context is worse than none. Tunable via
# $COORD_REHYDRATE_STALE_S for eval harnesses that need to bypass the
# freshness gate deterministically. Default: 24h.
stale_s="${COORD_REHYDRATE_STALE_S:-86400}"

now_md=""
should_inject=""
if [[ -n "$identity" ]]; then
  now_md="$coord_root/$identity/context/now.md"
  if [[ -f "$now_md" ]]; then
    # BSD + GNU stat fallback (same shape as pre-compact.sh).
    now_mtime="$(stat -f %m "$now_md" 2>/dev/null || stat -c %Y "$now_md" 2>/dev/null || echo 0)"
    if [[ "$now_mtime" =~ ^[0-9]+$ ]] && (( now_mtime > 0 )); then
      current_s="$(date +%s)"
      age_s=$(( current_s - now_mtime ))
      if (( age_s >= 0 && age_s < stale_s )); then
        should_inject="yes"
      fi
    fi
  fi
fi

# ─── Emit ────────────────────────────────────────────────────────────────
#
# We build the entire reminder as one stderr write. Claude Code
# surfaces our stderr verbatim, so any structure has to be inline.
#
# When context is fresh: <context> block first (so the model sees the
# state before the ritual instruction that would otherwise cause a
# blind reconstruct), then the ritual reminder.
#
# When context is absent/stale: only the ritual reminder. Identical
# to pre-brief-024 behavior — this hook remains backward-compatible
# with existing settings.local.json wirings.

{
  if [[ -n "$should_inject" ]]; then
    # `source="coord/context/now.md"` and `agent="<id>"` let downstream
    # consumers (evals, log analyzers) distinguish this synthetic block
    # from the model's own writes. Not consumed programmatically here —
    # purely informational for humans reading transcripts.
    printf '<context source="coord/context/now.md" agent="%s">\n' "$identity"
    cat "$now_md"
    # Ensure the closing tag lands on its own line even when now.md
    # doesn't end in a trailing newline.
    if [[ -n "$(tail -c 1 "$now_md" 2>/dev/null)" ]]; then
      printf '\n'
    fi
    printf '</context>\n\n'
  fi
  echo "Run the coord boot ritual: set status to available and drain inbox (read → reply → archive)."
} >&2

exit 2
