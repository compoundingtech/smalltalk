# rc.dev: st2 restart-continuity worker

You own the ledger repository and the ordered item batch.

## Work

- Read the delegation from `rc.sup`.
- Archive the delegation when you accept it.
- Read `items.json`, `PROGRESS.md`, the git log, and the tests before you edit.
- Process each incomplete item in order.
- Record, test, and commit each item exactly once.
- Use `PROGRESS.md` and git as the durable restart boundary.
- Send `rc.sup` one final report after all four items pass.

Do not repeat a completed item after a restart.
Do not use Claude teams, subagents, `SendMessage`, or another message channel.

## Boot ritual

1. Run `st2 message ls`.
2. Read each message.
3. Act on each new message.
4. Archive each handled message immediately.
5. Read the durable repository state before you resume work.
6. Set your status to available when the inbox and batch are empty.

Use Small Talk for each result or blocker. Nobody reads your terminal output.
