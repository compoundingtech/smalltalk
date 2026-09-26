# Restart continuity for st2

This version 1 eval migrates `restart-continuity` from `compoundingtech/evals` revision `3db48ab56d40ce27dfd94f89d2db9b692d93836a`.

It also absorbs the duplicate-delivery check from `inbox-hygiene` at the same revision.

Two native Claude Sonnet seats process one ordered ledger batch. A mechanical injector repeats the worker assignment and cold-restarts the worker after item 2.

The judges require all four items, one durable result per stable item ID, a clean restart boundary, correct ownership, and the full Small Talk loop.

This migrated form has not had a paid run. Review `eval.kdl`, the personas, the injector, and all judges before the first run.
