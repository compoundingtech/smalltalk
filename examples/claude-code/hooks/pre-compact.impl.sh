#!/bin/bash
# st PreCompact hook — brief-024 hook-legs.
#
# Fires just before Claude Code compacts the current context. Compaction
# wipes the model's in-context state; without a flush, everything the
# model knew about the current task is gone. This hook's job is to
# make sure `context/now.md` reflects reality *right now*, so the
# boot-rehydrate path (SessionStart) has fresh state to inject.
#
# HARD CONSTRAINT: exit 0 ALWAYS, no matter what. Compaction is a
# load-bearing operation — a hung or crashing hook that blocks it
# would wedge the entire session. All failure modes fall through to
# `exit 0`, and all error output is redirected to a file under
# `<st-root>/<identity>/context/.flush-errors.log`; NEVER stderr,
# because Claude Code surfaces our stderr as a system-reminder in the
# very next turn — which would flood every post-compaction context
# with hook chatter.
#
# What it actually does:
#   1. Refuses cleanly if $ST_AGENT / $ST_IDENTITY / $ST_AGENT
#      is unset (nothing meaningful to flush).
#   2. If `now.md` exists AND holds real (non-whitespace) content, we
#      trust that captured state and take NO action — we never clobber
#      it, even if it's stale. Real-but-stale state (decisions, open
#      threads) restores better than a reconstruct stub, and the read
#      side already declines to inject a now.md older than
#      $ST_REHYDRATE_STALE_S (default 24h), so staleness is handled there.
#   3. Only when now.md is ABSENT or EMPTY (missing, 0-byte, or
#      whitespace-only) does it write a "compaction-fired stub" via
#      `st context write` (atomic tmp+rename). Boot-rehydrate injects the
#      stub as a `<context>` block; the model sees "no fresh state was
#      captured, reconstruct from git+inbox."
#
# This is the EMPTY-only rule. The prior 5-minute mtime freshness gate
# stubbed any now.md older than 5 minutes — which CLOBBERED a
# good-but-6-minute-old now.md on compaction, losing real captured state.
# The former $ST_PRECOMPACT_FRESH_S threshold no longer applies.
#
# Hard cap on the `st context write` call via `timeout` (falls
# back to `gtimeout` on darwin systems with coreutils installed;
# falls through unbounded when neither is present — no exit-status
# change; the hook's other work should complete in single-digit ms
# anyway, so the cap is defense-in-depth, not a load-bearing
# invariant).
#
# Tunable via $ST_PRECOMPACT_TIMEOUT_S (seconds; default 5).
# Prior default was 0.5s, which flaked reliably under vitest's
# singleFork integration pool on darwin: cumulative process
# pressure (30+ spawned bashes per file, plus the ~4 fork+exec
# chain bash→pipe→timeout→PATH lookup→→bash) pushed the
# first flush test past the 500ms budget with exit 124 before
# the shim even wrote to its log. 5s is still an upper bound on a
# wedged CLI (well under any Claude Code hook-lifecycle limit) and
# doesn't affect real-world latency since a healthy write
# completes in ~ms.

set -uo pipefail

# ─── Binary resolution ───────────────────────────────────────────────────
#
# Prefer an absolute path injected by `st launch` via $ST_BIN (baked
# to the shim in the same package that generated this hook wiring).
# Falls back to PATH lookup for users who wired settings.local.json by
# hand or ran launch under a version that predates the injection. `st`
# is the canonical binary. Wired via $ST_BIN or `command -v st`.
# shim. Emptying $ST_BIN (unset or "") always falls through.
st_bin="${ST_BIN:-}"
if [[ -z "$st_bin" ]]; then
  st_bin="$(command -v st 2>/dev/null || true)"
fi

# ─── Identity + root resolution ──────────────────────────────────────────

identity="${ST_AGENT:-${ST_IDENTITY:-${ST_AGENT:-}}}"
if [[ -z "$identity" ]]; then
  # No identity → nothing to flush. Exit clean; do NOT write to stderr
  # (would inject a reminder into the compacted context).
  exit 0
fi

# `${HOME-}` (not `${HOME:-}`) so an unset HOME degrades to empty
# rather than tripping `set -u`. See the analogous comment in
# session-start.sh.
st_root="${ST_ROOT:-${ST_ROOT:-${HOME-}/.local/state/smalltalk}}"
context_dir="$st_root/$identity/context"
now_md="$context_dir/now.md"
err_log="$context_dir/.flush-errors.log"

# Ensure the context dir exists so we have somewhere to put the error
# log even before `st context write` lazy-creates it on first flush.
mkdir -p "$context_dir" 2>/dev/null || true

timeout_s="${ST_PRECOMPACT_TIMEOUT_S:-5}"

# ─── Preserve real state: only stub an ABSENT or EMPTY now.md ─────────────

# The stub is a fallback for when there is NOTHING to restore — it must
# never CLOBBER captured state. So skip the stub whenever now.md exists
# AND holds non-whitespace content, regardless of age. Rationale:
# real-but-stale state (decisions, open threads) is strictly more useful
# for a cold restore than a "reconstruct from scratch" stub, and staleness
# is already guarded on the READ side — the session-start hook declines to
# inject a now.md older than $ST_REHYDRATE_STALE_S (default 24h). Only an
# absent or effectively-empty now.md (missing, 0-byte, or whitespace-only)
# falls through to the stub write below.
#
# (Previously a 5-minute mtime freshness check, which CLOBBERED a
# good-but-6-minute-old now.md on compaction — a real data-loss bug. The
# former $ST_PRECOMPACT_FRESH_S threshold no longer applies.)
if [[ -f "$now_md" ]] && [[ -n "$(tr -d '[:space:]' < "$now_md" 2>/dev/null)" ]]; then
  exit 0
fi

# ─── Stub write ──────────────────────────────────────────────────────────

# Build the stub body once into a variable so we can pipe it into
# `st context write` under the timeout wrapper without dealing with
# the sub-shell / variable-scope headaches of `bash -c "$(declare -f)"`.
ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
# bash 3.2 (macOS /bin/bash, which Claude Code uses for hooks) cannot
# parse a heredoc nested inside `$( … )` — it fail-CLOSES the whole
# hook with a parse error. Read the heredoc into the var directly
# instead. `read -d ''` returns non-zero at EOF, hence `|| true`.
IFS= read -r -d '' stub_body <<EOF || true
# now — pre-compact stub — $ts

PreCompact fired without a recent flush from the model. Boot-rehydrate
should treat this as "no fresh state captured" and reconstruct from
git status, recent commits, and open inbox items.

If you see this after a restart: your last known-good working-state
was NOT captured before compaction. That's a discipline miss (the
base-persona rule is to flush \`st context write\` at each
meaningful state change), not a hook bug. Reconstruct and flush
proactively going forward.
EOF

# Run the write under a 500ms cap. `timeout` isn't universally present
# on darwin — try `timeout`, then `gtimeout`, then unbounded. Any
# failure of the write itself sinks to $err_log via the `2>>` on the
# pipeline. `|| true` on every branch guarantees we never inherit a
# non-zero status from the write.
# We do NOT pass `$identity` as a positional to `st context write`.
# Passing it positionally would trigger the anti-impersonation strict
# check in resolveAgent (explicit id → folder must pre-exist,
# AgentNotHostedError otherwise). Instead we rely on the env-var
# fallback path (ST_AGENT / ST_IDENTITY / ST_AGENT) — same
# resolution chain we used above to derive $identity, so the
# resolved id is guaranteed identical — which takes the implicit
# lazy-create branch and mkdirs `inbox/`+`archive/` on first flush.
# If $st_bin didn't resolve to anything, we can't flush. Log the miss
# and exit clean; compaction must not block on a missing CLI.
if [[ -z "$st_bin" ]]; then
  printf '%s pre-compact: no st binary found (checked $ST_BIN + PATH); skipping flush\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$err_log" 2>/dev/null || true
elif command -v timeout >/dev/null 2>&1; then
  printf '%s\n' "$stub_body" | \
    timeout "${timeout_s}s" "$st_bin" context write \
    > /dev/null 2>>"$err_log" || true
elif command -v gtimeout >/dev/null 2>&1; then
  printf '%s\n' "$stub_body" | \
    gtimeout "${timeout_s}s" "$st_bin" context write \
    > /dev/null 2>>"$err_log" || true
else
  printf '%s\n' "$stub_body" | \
    "$st_bin" context write \
    > /dev/null 2>>"$err_log" || true
fi

# Log rotation: if the error log has grown past 100 KB, truncate to
# the last ~50 KB so a stuck operator noticing "why is context/ full
# of errors" gets the recent tail, not gigabytes of history. Silent
# on failure — this is best-effort housekeeping.
if [[ -f "$err_log" ]]; then
  err_size="$(stat -f %z "$err_log" 2>/dev/null || stat -c %s "$err_log" 2>/dev/null || echo 0)"
  if [[ "$err_size" =~ ^[0-9]+$ ]] && (( err_size > 102400 )); then
    tail -c 51200 "$err_log" > "$err_log.tmp" 2>/dev/null && \
      mv "$err_log.tmp" "$err_log" 2>/dev/null || \
      rm -f "$err_log.tmp" 2>/dev/null
  fi
fi

# The prime directive: never block compaction. exit 0 unconditionally.
exit 0
