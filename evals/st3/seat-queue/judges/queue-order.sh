#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
test -s controller-state.json
source ./judges/history.sh

history="$(mktemp)"
trap 'rm -f "$history"' EXIT
dump_history "$history"

for key in alpha/draft alpha/publish bravo/work charlie/work; do
  subject="$(jq -er --arg key "$key" '.steps[$key]' controller-state.json)"
  st3 work show "$subject" --json | jq -e '.value.state == "completed"' >/dev/null
done

# Rebuild the seat's queue and each step's state from graph history at every
# claim the seat made, then require that the seat took the first ready step in
# queue order each time.
jq -se --slurpfile state controller-state.json '
  def status_at($subject; $index):
    [ .[] | select(
        .subject == $subject
        and .store_index < $index
        and (.kind == "step-run.state" or (.kind | startswith("work.")))
        and .body.fields.status != null
      ) ] | last | .body.fields.status;

  sort_by(.store_index) as $history
  | $state[0] as $s
  | $s.worker as $worker
  | [ "alpha/draft", "alpha/publish", "bravo/work", "charlie/work"
      | {key: ., subject: $s.steps[.], run: $s.runs[split("/")[0]]} ] as $seat
  | [ $s.runs.alpha, $s.runs.bravo, $s.runs.charlie ] as $runs
  | [ $history[] | select(.kind == "mission-run.created" and (.subject | IN($runs[]))) | .subject ] as $joins
  | [ $history[] | select(.kind == "agent.queue.moved" and .subject == $worker) ] as $moves
  | [ $history[] | select(.kind == "work.claimed" and .actor == $worker) ] as $claims
  | def order_at($index):
      reduce ($moves[] | select(.store_index < $index) | .body.fields) as $move ($joins;
        . as $order
        | map(select(. != $move.run)) as $rest
        | if $move.placement == "top" then [$move.run] + $rest
          elif $move.placement == "bottom" then $rest + [$move.run]
          else ($rest | index($move.anchor)) as $at
            | if $at == null then $order
              elif $move.placement == "before" then $rest[:$at] + [$move.run] + $rest[$at:]
              else $rest[:$at + 1] + [$move.run] + $rest[$at + 1:]
              end
          end);
    def created($subject):
      [ $history[] | select(.subject == $subject) | .store_index ] | min;
    def next_at($index):
      order_at($index) as $order
      | [ $seat[]
          | .subject as $subject
          | .run as $run
          | select(($history | status_at($subject; $index)) == "ready")
          | {subject: $subject, rank: ($order | index($run) // 1000000), created: created($subject)} ]
      | sort_by(.rank, .created) | first | .subject;
    def claimed_at($key):
      [ $claims[] | select(.subject == $s.steps[$key]) | .store_index ] | first;
    def ready_at($key):
      [ $history[] | select(
          .subject == $s.steps[$key]
          and .kind == "step-run.state"
          and .body.fields.status == "ready"
        ) | .store_index ] | first;

  # Start order is the default queue order.
  $joins == $runs

  # The authorized agent, not the seat and not a person, moved charlie before
  # bravo.
  and ($s.mover | startswith("agent/"))
  and $s.mover != $worker
  and ($moves | length) == 1
  and $moves[0].actor == $s.mover
  and $moves[0].body.fields.run == $s.runs.charlie
  and $moves[0].body.fields.placement == "before"
  and $moves[0].body.fields.anchor == $s.runs.bravo
  and ($moves[0].body.fields.reason | type) == "string"

  # The seat claimed exactly its four steps, once each, in the moved order with
  # the waiting head run passed over and then taken again.
  and [ $claims[].subject ] == [ $s.steps["alpha/draft", "charlie/work", "alpha/publish", "bravo/work"] ]

  # Each claim was the first ready step in the queue at that moment.
  and all($claims[]; .subject as $subject | .store_index as $index | $subject == next_at($index))

  # The move, not readiness, put charlie first: bravo joined earlier and was
  # already ready when the seat took charlie.
  and $moves[0].store_index < claimed_at("charlie/work")
  and ($history | status_at($s.steps["bravo/work"]; claimed_at("charlie/work"))) == "ready"

  # Alpha was the head run and waiting when the seat passed over it.
  and (order_at(claimed_at("charlie/work")) | first) == $s.runs.alpha
  and ($history | status_at($s.steps["alpha/publish"]; claimed_at("charlie/work"))) != "ready"

  # Alpha became ready later, and the seat returned to it before bravo.
  and ready_at("alpha/publish") > claimed_at("charlie/work")
  and ready_at("alpha/publish") < claimed_at("alpha/publish")
  and claimed_at("alpha/publish") < claimed_at("bravo/work")
  and ($history | status_at($s.steps["bravo/work"]; claimed_at("alpha/publish"))) == "ready"
' "$history" >/dev/null

echo "PASS: the seat claimed each step as the queue's next work, followed the moved order, passed over the waiting head run, and returned to it once ready"
