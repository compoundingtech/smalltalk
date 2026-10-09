#!/usr/bin/env bash
# Run the onboarding-play eval. With no flags it spends nothing: it checks the judge against its
# fixtures and prints the plan. A paid run needs --paid and a run cap:
#
#   evals/st3/onboarding-play/run.sh --paid --max-runs 2 --harness claude \
#     --claude-config-dir /path/to/a/login/you/made --st-bin target/debug/st3
#
# Each run starts a throwaway st (own HOME, own daemon, no service) from the candidate binary,
# lets the Assistant play the onboarding with a person who never types (the `silent` variant) or
# who says "skip" during act one (`skip`), collects the evidence from the graph, and judges it.
# Nothing is copied from your real home: the harness login comes only from the directory you name,
# and no API key file is read. The first sign of a login or rate-limit problem stops everything.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
paid=false; max_runs=0; harness=claude; variants="silent,skip"; st_bin="${ST_BIN:-}"
claude_dir=""; codex_home=""; out="$here/reports/run-$(date -u +%Y%m%dT%H%M%SZ)"
overall_s=900; person="person/avery"; plumbing=false
usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --paid) paid=true ;;
    --plumbing) plumbing=true ;;
    --max-runs) max_runs="${2:?}"; shift ;;
    --harness) harness="${2:?}"; shift ;;
    --variants) variants="${2:?}"; shift ;;
    --st-bin) st_bin="${2:?}"; shift ;;
    --claude-config-dir) claude_dir="${2:?}"; shift ;;
    --codex-home) codex_home="${2:?}"; shift ;;
    --out) out="${2:?}"; shift ;;
    --timeout) overall_s="${2:?}"; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown flag: $1" >&2; usage 2 ;;
  esac
  shift
done

"$here/self-test.sh"

if $plumbing; then
  # Model-free, live: a throwaway st with no harness, so no Assistant ever starts and nothing is
  # spent. Proves the collector runs against a real graph and that the judge fails an empty play.
  [ -x "${st_bin:-}" ] || { echo "--plumbing needs --st-bin" >&2; exit 2; }
  root="$(mktemp -d)"; mkdir -p "$root/home/.config" "$root/home/.local/state" "$root/home/.local/bin" "$root/run"
  # What `st setup --install` would put in ~/.local/bin, linked rather than copied: the candidate
  # binary and the pty on this machine's PATH.
  ln -s "$st_bin" "$root/home/.local/bin/st3"; ln -s "$st_bin" "$root/home/.local/bin/st"
  ln -s "$(command -v pty)" "$root/home/.local/bin/pty"
  plumb_env=(env -i PATH="$root/home/.local/bin:$PATH" HOME="$root/home" USER=avery LANG="${LANG:-C.UTF-8}"
    XDG_CONFIG_HOME="$root/home/.config" XDG_STATE_HOME="$root/home/.local/state" XDG_RUNTIME_DIR="$root/run"
    ST_BIN="$st_bin")
  cleanup_plumbing() {
    local pid
    for pid in $(pgrep -u "$(id -u)" -x st3 || true); do
      if tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -qx "HOME=$root/home"; then kill "$pid" 2>/dev/null || true; fi
    done
  }
  trap cleanup_plumbing EXIT
  "${plumb_env[@]}" "$st_bin" setup --person avery --node evalbox --yes --install false --service false \
    --start true --harness none >"$root/setup.log" 2>&1 || { cat "$root/setup.log" >&2; exit 1; }
  mkdir -p "$out"
  "${plumb_env[@]}" "$here/collect.sh" silent "$(date +%s%N | cut -c1-13)" none >"$out/plumbing.json"
  jq -e '.variant == "silent" and (.assistant_text|length) == 0 and .demo_agent_exists == false' "$out/plumbing.json" >/dev/null
  if "$here/judge.sh" "$out/plumbing.json" >"$out/plumbing.txt"; then echo "an empty play must not pass" >&2; exit 1; fi
  echo "plumbing ok: the collector built a bundle from a live graph and the judge rejected the empty play ($out/plumbing.json)"
  exit 0
fi

if ! $paid; then
  echo "model-free checks passed; no paid run requested (add --paid --max-runs N to run for real)"
  exit 0
fi

# ---- guards before any spend ------------------------------------------------------------------
[ "$max_runs" -ge 1 ] 2>/dev/null || { echo "--paid needs --max-runs N with N >= 1" >&2; exit 2; }
[ -x "${st_bin:-}" ] || { echo "--st-bin must name the candidate st binary" >&2; exit 2; }
case "$harness" in
  claude) [ -d "$claude_dir" ] || { echo "--claude-config-dir must name a directory you logged in to" >&2; exit 2; } ;;
  codex) [ -d "$codex_home" ] || { echo "--codex-home must name a directory you logged in to" >&2; exit 2; } ;;
  *) echo "--harness must be claude or codex" >&2; exit 2 ;;
esac
IFS=, read -r -a plan <<<"$variants"
[ "${#plan[@]}" -le "$max_runs" ] || plan=("${plan[@]:0:$max_runs}")
echo "paid onboarding eval: harness=$harness runs=${#plan[@]} (cap $max_runs) out=$out"
mkdir -p "$out"

now_ms() { date +%s%N | cut -c1-13; }
# The login directory is shared with real work: setup must not register a plugin marketplace from
# the throwaway home inside it, so Claude seats use the inline development channel.
harness_args=(); [ "$harness" = claude ] && harness_args=(--claude-channel false)
trouble='(rate.?limit|usage limit|limit reached|quota|not logged in|please log in|/login|log in to|authentication|unauthori[sz]ed|invalid api key)'

stop_throwaway() { # kill the daemon this run started: the st3 whose environment holds our HOME
  local pid
  for pid in $(pgrep -u "$(id -u)" -x st3 || true); do
    if tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -qx "HOME=$1/home"; then kill "$pid" 2>/dev/null || true; fi
  done
}

failures=0; ran=0
for variant in "${plan[@]}"; do
  ran=$((ran + 1))
  root="$(mktemp -d)"; mkdir -p "$root/home/.config" "$root/home/.local/state" "$root/run"
  run_env=(env -i PATH="$PATH" HOME="$root/home" USER=avery LANG="${LANG:-C.UTF-8}" TERM=xterm-256color
    XDG_CONFIG_HOME="$root/home/.config" XDG_STATE_HOME="$root/home/.local/state" XDG_RUNTIME_DIR="$root/run"
    ST_BIN="$st_bin" EVAL_PERSON="$person")
  [ -n "$claude_dir" ] && run_env+=(CLAUDE_CONFIG_DIR="$claude_dir")
  [ -n "$codex_home" ] && run_env+=(CODEX_HOME="$codex_home")
  st() { "${run_env[@]}" "$st_bin" "$@"; }
  echo "== run $ran/${#plan[@]}: $variant (throwaway $root)"
  started="$(now_ms)"; skip_ms=none; verdict=""
  st setup --person "${person#person/}" --node evalbox --yes --install false --service false \
    --start true --harness "$harness" "${harness_args[@]}" >"$out/$variant.setup.log" 2>&1 || verdict="setup failed"
  deadline=$(( $(date +%s) + overall_s ))
  while [ -z "$verdict" ] && [ "$(date +%s)" -lt "$deadline" ]; do
    sleep 5
    # Stop everything at the first login or rate-limit trouble; never retry a paid run.
    seen="$(st agents show agent/st/assistant --json 2>/dev/null | jq -r '[.value.state, .value.harness_state] | map(. // "") | join(" ")' || true)"
    if grep -Eqi "login|auth|rate|limit" <<<"$seen" || grep -Eqi "$trouble" "$out/$variant.setup.log"; then
      verdict="stopped: login or rate-limit trouble ($seen)"; stop_all=true; break
    fi
    run_state="$(st missions ls --all --json 2>/dev/null | jq -r '[.. | objects | select((.id? // "") | test("^st/onboarding(/|$)"))] | first | .status // .state // empty' || true)"
    case "$variant" in
      skip)
        if [ "$skip_ms" = none ] && st agents show agent/st/demo --json 2>/dev/null \
            | jq -e '(.value.operational.reasons // []) | index("undeclared") | not' >/dev/null; then
          sleep 5; skip_ms="$(now_ms)"
          st conversations send agent/st/assistant --from "$person" --body skip >/dev/null 2>&1 || true
        fi
        [ "$skip_ms" != none ] && [ "$run_state" = cancelled ] && break ;;
      silent) case "$run_state" in completed|cancelled|failed) break ;; esac ;;
    esac
  done
  # A silent person leaves the Assistant to its default; give it the window after its last question.
  [ "$variant" = silent ] && [ -z "$verdict" ] && sleep 60
  if [ "${verdict#stopped}" = "$verdict" ] && [ "$verdict" != "setup failed" ]; then
    "${run_env[@]}" "$here/collect.sh" "$variant" "$started" "$skip_ms" >"$out/$variant.json" 2>"$out/$variant.collect.log" \
      || verdict="collection failed (see $variant.collect.log)"
    if [ -z "$verdict" ]; then
      if "$here/judge.sh" "$out/$variant.json" | tee "$out/$variant.txt"; then verdict=pass; else verdict=fail; fi
    fi
  fi
  # The tokens the throwaway graph recorded for this run, kept beside the verdict (counts only).
  st usage --hours 3 --json >"$out/$variant.usage.json" 2>/dev/null || true
  st agents stop agent/st/assistant >/dev/null 2>&1 || true
  stop_throwaway "$root"
  echo "   -> $variant: $verdict"; echo "$variant: $verdict" >>"$out/summary.txt"
  [ "$verdict" = pass ] || failures=$((failures + 1))
  if [ "${stop_all:-false}" = true ]; then echo "stopping: first login or rate-limit trouble" >&2; break; fi
done
echo "summary in $out/summary.txt ($failures not passing)"
[ "$failures" -eq 0 ]
