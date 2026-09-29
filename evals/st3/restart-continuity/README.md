# Restart continuity for st3

This version 2 eval migrates `restart-continuity` from `compoundingtech/evals` revision `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.

It also absorbs the duplicate-delivery check from `inbox-hygiene` at the same revision.

The st3 graph owns the complete mission before work starts. It records two assigned work phases, the cold restart, five stable products, nested item progress, verification, and cleanup.

A mechanical injector restarts `rc.dev` after item 2. It sends one repeated Small Talk delivery through the st3 API after the new incarnation becomes ready.

The worker must use the graph, `PROGRESS.md`, and git history to resume with item 3. The gates reject a skipped item, a repeated stable item result, a missing product, incorrect ownership, or an invalid message sequence.

The injector reads the agent card for the replacement incarnation and records the exact duplicate
message subject. `variants/` changes only rc.dev to omp on `openai-codex/gpt-5.6-luna` or Codex
`gpt-6-luna`; the committed `eval.kdl` is the Claude `claude-sonnet-5` form. The reports from
2026-09-26 record paid runs of all three.
