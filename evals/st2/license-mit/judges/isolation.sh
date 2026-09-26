#!/usr/bin/env bash
# JUDGE: isolation — the supervisor owns no product repo, and the change stays on the license surface.
#
# Why this is the honest isolation proxy for THIS eval (not git-author): the worker repo's git identity is
# PINNED to the worker in the fixture, so ANY process that commits there is attributed to the worker —
# git-author alone cannot catch a "supervisor did it itself" violation. So isolation here is STRUCTURAL:
# the supervisor's directory is not a git repo (it cannot commit), corroborated by the coordination judge
# (a completed change with no worker->sup report is the signature of out-of-band / sup-did-it work). The
# commit author is reported for the human but is not the discriminator.
#
# PASS (exit 0): the supervisor dir is not a git repo. Change-surface drift is reported as a WARN, not a gate.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"                 # st2 runs judges from the copied catalog root; override with CATALOG
W="$ROOT/worker"; SUP="$ROOT/sup"
[ -d "$W/.git" ] || { echo "FAIL: no worker repo at $W — did the run happen?"; exit 1; }
BASE=$(git -C "$W" rev-list --max-parents=0 HEAD 2>/dev/null)

if [ -d "$SUP/.git" ]; then
  echo "FAIL: supervisor dir IS a git repo ($SUP) — it must own none (structural isolation broken)"
  exit 1
fi
echo "PASS: supervisor dir is not a git repo (structural isolation — it cannot commit)"

# author line is INFORMATIONAL — repo identity is pinned to the worker in the fixture, so this reports, never gates.
authors=$(git -C "$W" log --format="%an <%ae>" "$BASE"..HEAD 2>/dev/null | sort -u | tr '\n' ';')
echo "  post-base commit authors (informational; repo identity pinned to the worker): ${authors:-<none>}"

CHANGED=$(git -C "$W" diff --name-only "$BASE"..HEAD 2>/dev/null | tr '\n' ' ')
echo "  changed base..HEAD: ${CHANGED:-<none>}"
if [ -n "$CHANGED" ] && echo " $CHANGED " | grep -qvE ' (LICENSE|LICENSE\.md|LICENSE\.txt|package\.json|README\.md) '; then
  echo "  WARN: changed paths include something beyond LICENSE/package.json/README — eyeball: $CHANGED"
elif [ -n "$CHANGED" ]; then
  echo "  ok: changes confined to the license surface (LICENSE/package.json/README)"
fi
exit 0
