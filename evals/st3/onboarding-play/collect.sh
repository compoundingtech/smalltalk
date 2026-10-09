#!/usr/bin/env bash
# Build one evidence bundle from a live onboarding run. Everything it reads is the graph and the
# Assistant's own conversation; the Assistant's claims are never taken as evidence.
# Usage: collect.sh VARIANT STARTED_MS SKIP_MS_OR_NONE   (ST_BIN and the isolated environment set)
set -euo pipefail

variant="${1:?variant}"; started_ms="${2:?started_ms}"; skip_ms="${3:-none}"
st="${ST_BIN:-st}"
assistant="agent/st/assistant"; demo="agent/st/demo"; person="${EVAL_PERSON:-person/avery}"
now_ms() { date +%s%N | cut -c1-13; }

# The Assistant's conversation: what it printed for the person and what the person sent it.
session="$("$st" conversations sessions --as "$assistant" --json \
  | jq -r --arg a "$assistant" '[.value.items[] | select(.owner_id == $a)] | sort_by(.started_at) | last | .id // empty')"
timeline='[]'
if [ -n "$session" ]; then
  timeline="$("$st" conversations timeline "$session" --as "$assistant" --json --limit 500 \
    | jq '[.value.items[] | select(.type == "content" and .final == true and (.body.text // "") != "")
           | {ms: ((.timestamp | sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601) * 1000), role, text: .body.text}]')"
fi
# Assistant text is what it said as itself; the person's text arrived as a message from the person.
assistant_text="$(jq '[.[] | select(.role == "assistant") | {ms, text}]' <<<"$timeline")"
person_messages="$(jq --arg p "$person" '[.[] | select(.role == "user" and (.text | contains("from=\"" + $p + "\""))) | {ms, text}]' <<<"$timeline")"

# Messages between the two agents, timed from each message's age: sent = now - age.
now="$(now_ms)"
messages='[]'
for mailbox in "$assistant" "$demo"; do
  rows="$("$st" conversations ls "$mailbox" --archive --json 2>/dev/null || echo '[]')"
  for id in $(jq -r '.[] | select((.from | startswith("agent/")) and (.to | startswith("agent/"))) | .subject' <<<"$rows"); do
    age="$("$st" conversations status "$id" --json | jq -r '.delivery.age_ms // 0')"
    messages="$(jq --argjson m "$(jq --arg id "$id" --argjson now "$now" --argjson age "$age" \
      '.[] | select(.subject == $id) | {id: .subject, from, to, ms: ($now - $age)}' <<<"$rows")" \
      '. + [$m]' <<<"$messages")"
  done
done
messages="$(jq 'unique_by(.id)' <<<"$messages")"

# `agents show` answers for a seat that was never declared (reason "undeclared"), so ask the graph
# whether the declaration exists rather than whether the command worked.
demo_agent_exists=false
"$st" agents show "$demo" --all --json 2>/dev/null \
  | jq -e '(.value.operational.reasons // []) | index("undeclared") | not' >/dev/null && demo_agent_exists=true
demo_mission_state="$("$st" missions ls --all --json 2>/dev/null \
  | jq -r '[.. | objects | select((.id? // "") | test("st/onboarding-demo"))] | first | .status // .state // empty' || true)"
welcome_bytes=0
demo_workspace="${HOME}/st/agents/st-demo"
[ -f "$demo_workspace/welcome.md" ] && welcome_bytes="$(wc -c <"$demo_workspace/welcome.md" | tr -d ' ')"
onboarding_run_state="$("$st" missions ls --all --json 2>/dev/null \
  | jq -r '[.. | objects | select((.id? // "") | test("^st/onboarding(/|$)"))] | first | .status // .state // empty' || true)"

jq -n \
  --arg variant "$variant" --argjson started "$started_ms" \
  --argjson skip "$([ "$skip_ms" = none ] && echo null || echo "$skip_ms")" \
  --argjson assistant_text "$assistant_text" --argjson person_messages "$person_messages" \
  --argjson messages "$messages" --argjson demo_exists "$demo_agent_exists" \
  --arg demo_state "${demo_mission_state:-}" --argjson welcome "$welcome_bytes" \
  --arg run_state "${onboarding_run_state:-}" \
  '{variant: $variant, started_ms: $started, skip_ms: $skip, assistant_text: $assistant_text,
    person_messages: $person_messages, messages: $messages, demo_agent_exists: $demo_exists,
    demo_mission_state: (if $demo_state == "" then null else $demo_state end),
    welcome_bytes: $welcome, onboarding_run_state: (if $run_state == "" then null else $run_state end)}'
