#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
test -s controller-state.json
source ./judges/history.sh

history="$(mktemp)"
trap 'rm -f "$history"' EXIT
dump_history "$history"

jq -se --slurpfile state controller-state.json '
  sort_by(.store_index) as $history
  | $state[0] as $s
  | $s.worker as $worker
  | [ $s.steps["alpha/draft", "alpha/publish", "bravo/work", "charlie/work"] ] as $seat
  | def on($subject; $kind): [ $history[] | select(.subject == $subject and .kind == $kind) ];
    def held($subject):
      { subject: $subject,
        claimed: (on($subject; "work.claimed") | first | .store_index),
        submitted: (on($subject; "work.submitted") | first | .store_index),
        completed: ([ on($subject; "step-run.state")[] | select(.body.fields.status == "completed") ]
          | first | .store_index) };
    [ $seat[] | held(.) ] as $holds
  | [ $history[] | select(.kind == "agent.queue.moved" and .subject == $worker) ] as $moves
  | [ $history[]
      | select(.kind == "message.sent" and .body.fields.to == $worker)
      | .store_index as $index
      | .body.fields.tags[]?
      | select(startswith("st3-work:"))
      | ltrimstr("st3-work:") | split("@") | .[0]
      | {step: ., index: $index} ] as $wakes
  | ($holds[] | select(.subject == $s.steps["alpha/draft"])) as $draft

  # Each seat step was claimed once by the seat, never released, failed, or
  # retried, and was submitted by the seat before it completed.
  | all($seat[];
      . as $subject
      | (on($subject; "work.claimed") | length == 1 and .[0].actor == $worker)
      and (on($subject; "work.submitted") | length == 1 and .[0].actor == $worker)
      and (on($subject; "work.released") | length == 0)
      and (on($subject; "work.failed") | length == 0)
      and (on($subject; "step-run.retried") | length == 0))
  and all($holds[]; .claimed < .submitted and .submitted <= .completed)

  # Between claim and submission a step only moved between claimed and working.
  and all($holds[];
      . as $hold
      | all($history[]
          | select(
              .subject == $hold.subject
              and .store_index > $hold.claimed
              and .store_index < $hold.submitted
              and .body.fields.status != null
            );
          .body.fields.status == "claimed" or .body.fields.status == "working"))

  # The seat never held two claims at once.
  and ([ $holds | sort_by(.claimed) | range(1; length) as $i | .[$i].claimed > .[$i - 1].submitted ] | all)

  # The person moved a run while the seat held alpha'"'"'s draft, and the draft
  # stayed with the seat until the seat submitted it.
  and ($moves | length) == 1
  and $draft.claimed < $moves[0].store_index
  and $moves[0].store_index < $draft.submitted

  # No work wake reached the seat while it held a different step.
  and ($wakes | length) >= 1
  and all($wakes[];
      . as $wake
      | all($holds[] | select(.subject != $wake.step);
          ($wake.index < .claimed) or ($wake.index > .completed)))
' "$history" >/dev/null

echo "PASS: every held claim stayed with the seat until it submitted the work; the move and later wakes preempted nothing"
