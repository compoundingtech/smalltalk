# Seat queue

This paid black-box eval keeps one durable top-level seat, `agent/eval/seat-queue/worker`, on a
real Claude TUI with `claude-sonnet-5`. Three finite missions assign work to that one seat.

`scripts/st3-seat-queue-eval/run` repeats the eval on fresh isolated daemons. Each run names its
seat: `claude` keeps the harness in `eval.kdl`, `codex` swaps in a Codex TUI with `gpt-6-luna`, and
`omp` swaps in omp with `openai-codex/gpt-5.6-luna`. The controller and judges are the same for
every seat.

The controller starts the runs in a known order: `alpha`, `bravo`, then `charlie`. Alpha has two
seat steps with an agentless human sign-off gate between them. Bravo and charlie each have one
seat step.

A second top-level agent, `agent/eval/seat-queue/chief`, is model-free. Its declaration grants
`queue-authority { move "eval/seat-queue/worker" }`, the way `mission-authority` grants a seat
named missions. The controller acts as the chief, as the mission-authority eval acts as its
planner. It first tries the move as the worker, which has no grant, and requires
`queue-authority-denied`. Then, while the seat holds alpha's first step, the chief moves charlie
before bravo with `st3 agents queue move --as agent/eval/seat-queue/chief`. Alpha then waits on
its sign-off gate as the head run. The controller approves the gate only after the seat has taken
charlie's work, while the seat still holds that work.

The controller only starts runs, moves one run, approves one gate, and observes. It never claims,
completes, or wakes seat work. Every wake comes from the reconciler and reaches the agent through
native delivery.

The first live runs showed that the wake and the agent's own work list are not enough. One seat
finished a step, read its work list, and claimed a later run's step before the head run's wake
arrived. `work claim` now refuses a later run's step while an earlier run has ready work for the
seat. This eval checks, with a live agent, that the seat ends in queue order.

The held-out judges rebuild the seat queue and each step's state from graph history at every claim.
They pass only if the seat did all of the following:

- claimed each step when it was the first ready step in queue order;
- took charlie before bravo because of the one move, which the chief made;
- passed over waiting alpha, and took alpha's second step before bravo once alpha was ready;
- kept each held claim until it submitted that work, and got no wake for other work while it held a
  claim;
- received no terminal input.

If the agent finishes a step before the controller's move or gate approval lands, the controller
fails and names the timing. That result says nothing about the agent or st3. Run the eval again.
