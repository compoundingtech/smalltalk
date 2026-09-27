#!/usr/bin/env bash
# PROJECT scope inherited (hard gate): SKILL_PROJECT.txt in the worker's cwd carries the project skill's secret
# token — extracted from the skill's OWN body (ground truth), which the kick never named. Its presence proves the
# repo/.claude/skills skill actually LOADED + FIRED (the worker had no other way to learn the token).
set -uo pipefail
SB="${CATALOG:?CATALOG not set}"; REPO="$SB/repo"
want="$(grep -oE 'SIP-[a-z0-9-]+' "$REPO/.claude/skills/evalskill-project/SKILL.md" 2>/dev/null | head -1)"
[ -n "$want" ] || { echo "FAIL: could not read the project skill's token from its SKILL.md"; exit 1; }
f="$REPO/SKILL_PROJECT.txt"
[ -f "$f" ] || { echo "FAIL: SKILL_PROJECT.txt absent — the project-scope skill never fired (not inherited / not invoked)"; exit 1; }
got="$(tr -d '[:space:]' < "$f")"
[ "$got" = "$want" ] || { echo "FAIL: SKILL_PROJECT.txt token mismatch (got '$got', want '$want') — not produced by the real skill body"; exit 1; }
echo "PASS: SKILL_PROJECT.txt carries the project skill's secret token ($want) — PROJECT scope inherited from repo/.claude/skills + invoked"
