#!/usr/bin/env bash
# Score one onboarding-play evidence bundle. Pure: it reads JSON and decides, so it can be tested
# without a harness. Usage: judge.sh evidence.json   (exit 0 pass, 1 fail, 2 unusable evidence)
set -euo pipefail

evidence="${1:?usage: judge.sh evidence.json}"
first_message_budget_s="${FIRST_MESSAGE_BUDGET_S:-180}"
default_min_s="${DEFAULT_MIN_S:-25}"
default_max_s="${DEFAULT_MAX_S:-150}"
skip_budget_s="${SKIP_BUDGET_S:-60}"
min_you="${MIN_YOU:-5}"

jq -e '.variant and (.started_ms|type=="number") and (.assistant_text|type=="array")' "$evidence" >/dev/null \
  || { echo "unusable evidence: $evidence" >&2; exit 2; }

# What the person reads must never name these. Matches are case-insensitive except Home and Ada,
# which are only a problem as proper words.
banned='(attention item|step-run|work step|mission step|\bgates?\b|\bverif(y|ies|ying|ied|ication)\b|the graph|st conversations|st ui |person/|the person|the user)'

jq --argjson first "$first_message_budget_s" \
   --argjson dmin "$default_min_s" --argjson dmax "$default_max_s" \
   --argjson skipb "$skip_budget_s" --argjson minyou "$min_you" \
   --arg banned "$banned" '
  def assistant_msgs: [.messages[] | select(.from == "agent/st/assistant" and .to == "agent/st/demo")];
  def demo_msgs: [.messages[] | select(.from == "agent/st/demo" and .to == "agent/st/assistant")];
  def all_text: ([.assistant_text[].text] | join("\n"));
  def check($name; $pass; $detail): {name: $name, pass: $pass, detail: $detail};
  . as $e
  | ([.messages[] | select((.from|startswith("agent/")) and (.to|startswith("agent/"))) | .ms] | min) as $first_ms
  | (all_text) as $text
  | ([.assistant_text[] | select(.text | test("what do you want to accomplish"; "i")) | .ms] | min) as $ask_ms
  | (if .variant == "silent" then
      [ check("the first agent-to-agent message arrives in time";
              ($first_ms != null and ((($first_ms - $e.started_ms) / 1000) <= $first));
              (if $first_ms == null then "no agent-to-agent message"
               else "\((($first_ms - $e.started_ms) / 1000)) s after start, budget \($first) s" end)),
        check("act one: the second agent exists and both agents wrote";
              (.demo_agent_exists == true and (assistant_msgs|length) > 0 and (demo_msgs|length) > 0);
              "assistant to demo \(assistant_msgs|length), demo to assistant \(demo_msgs|length), seat \(.demo_agent_exists)"),
        check("act two: the demo mission completed and left its note";
              (.demo_mission_state == "completed" and (.welcome_bytes // 0) > 0);
              "mission \(.demo_mission_state), welcome.md \(.welcome_bytes // 0) bytes"),
        check("the person stayed silent and the Assistant carried on after the question";
              (($e.person_messages | length) == 0 and $ask_ms != null
               and ([$e.assistant_text[] | select(.ms >= $ask_ms + ($dmin * 1000) and .ms <= $ask_ms + ($dmax * 1000))] | length) > 0);
              (if $ask_ms == null then "the interview question was never asked"
               else "person messages \($e.person_messages|length); later assistant text in the window \($dmin)-\($dmax) s: \([$e.assistant_text[] | select(.ms >= $ask_ms + ($dmin * 1000) and .ms <= $ask_ms + ($dmax * 1000))] | length)" end))
      ]
    else
      ( ($e.skip_ms // null) as $skip
      | [ check("the person said skip";
                ($skip != null and ($e.person_messages | map(select(.text | test("skip"; "i"))) | length) > 0);
                "skip at \($skip)"),
          check("the Assistant answered the skip in time and said how to come back";
                ($skip != null and ([$e.assistant_text[] | select(.ms >= $skip and .ms <= $skip + ($skipb * 1000) and (.text | test("st setup --onboarding")))] | length) > 0);
                "within \($skipb) s of the skip"),
          check("the demo mission is not left running and no agent is messaged after the skip";
                ($e.demo_mission_state != "running"
                 and ($skip != null and ([assistant_msgs[] | select(.ms > $skip + ($skipb * 1000))] | length) == 0));
                "mission \($e.demo_mission_state)"),
          check("the onboarding run was cancelled";
                ($e.onboarding_run_state == "cancelled");
                "run \($e.onboarding_run_state)")
        ])
    end) as $variant_checks
  | [ check("the text is in the second person";
            ((($text | [match("\\byou\\b"; "gi")] | length) >= $minyou) and ($text | test("\\bHome\\b") | not));
            "\($text | [match("\\byou\\b"; "gi")] | length) uses of you, Home mentioned: \($text | test("\\bHome\\b"))"),
      check("the text never names steps, gates, the graph, attention items or a sample person";
            (($text | test($banned; "i") | not) and ($text | test("\\bAda\\b") | not));
            (($text | [match($banned; "gi") | .string] | unique | join(", ")) as $hits
             | if $hits == "" then "clean" else "found: \($hits)" end))
    ] + $variant_checks
  | {variant: $e.variant,
     first_agent_message_seconds: (if $first_ms == null then null else (($first_ms - $e.started_ms) / 1000) end),
     checks: ., pass: (all(.[]; .pass))}
' "$evidence" | tee "${evidence%.json}.result.json" | jq -r '
  "variant: \(.variant)  first agent message: \(.first_agent_message_seconds // "none") s",
  (.checks[] | "  \(if .pass then "PASS" else "FAIL" end)  \(.name) — \(.detail)"),
  (if .pass then "PASS" else "FAIL" end)'

jq -e '.pass' "${evidence%.json}.result.json" >/dev/null
