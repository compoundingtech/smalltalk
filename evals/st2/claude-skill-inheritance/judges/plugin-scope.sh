#!/usr/bin/env bash
# PLUGIN scope inherited — UNION across scopes (hard gate): SKILL_PLUGIN.txt carries the plugin skill's secret
# token, extracted from the plugin skill's OWN body (ground truth). The plugin was loaded via `claude --plugin-dir`
# (session-only) and its skill is namespaced (evalpkg:evalskill-plugin); the token proves it LOADED + FIRED
# alongside the project skill — the worker saw BOTH scopes at once.
set -uo pipefail
SB="${CATALOG:?CATALOG not set}"; REPO="$SB/repo"
want="$(grep -oE 'SIU-[a-z0-9-]+' "$SB/plugin/evalpkg/skills/evalskill-plugin/SKILL.md" 2>/dev/null | head -1)"
[ -n "$want" ] || { echo "FAIL: could not read the plugin skill's token from its SKILL.md"; exit 1; }
f="$REPO/SKILL_PLUGIN.txt"
[ -f "$f" ] || { echo "FAIL: SKILL_PLUGIN.txt absent — the plugin-scope skill never fired (--plugin-dir not honored / not invoked)"; exit 1; }
got="$(tr -d '[:space:]' < "$f")"
[ "$got" = "$want" ] || { echo "FAIL: SKILL_PLUGIN.txt token mismatch (got '$got', want '$want') — not produced by the real plugin skill body"; exit 1; }
echo "PASS: SKILL_PLUGIN.txt carries the plugin skill's secret token ($want) — PLUGIN scope inherited via --plugin-dir (UNION with project) + invoked"
