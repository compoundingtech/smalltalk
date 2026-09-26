#!/usr/bin/env bash
# ISOLATION (hard gate — the eval polluted nothing shared): the eval must NOT have written evalskill-* into the
# operator's ~/.claude/skills, nor cached evalpkg into ~/.claude/plugins (`--plugin-dir` is session-only). Both are
# read-only checks on the real ~/.claude; project skills are cwd-local, so personal scope stays untouched.
set -uo pipefail
pol_sk="$(ls "$HOME/.claude/skills" 2>/dev/null | grep -c 'evalskill' || true)"
[ "${pol_sk:-0}" = 0 ] || { echo "FAIL: $pol_sk evalskill-* under ~/.claude/skills — the eval polluted personal scope"; exit 1; }
pol_pl="$(find "$HOME/.claude/plugins" -maxdepth 4 -iname 'evalpkg*' 2>/dev/null | grep -c . || true)"
[ "${pol_pl:-0}" = 0 ] || { echo "FAIL: evalpkg cached under ~/.claude/plugins ($pol_pl) — --plugin-dir was not session-local"; exit 1; }
echo "PASS: no evalskill-* under ~/.claude/skills and no evalpkg under ~/.claude/plugins — personal scope untouched (project skills cwd-local, --plugin-dir session-only)"
