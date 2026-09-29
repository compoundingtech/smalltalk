# Graceful messaging

This paid black-box eval proves that a running seat keeps its messages across a deploy. A deploy
installs a new st binary at the same path and then restarts the daemon. Before this eval, a seat's
driver kept executing the deleted binary, and on 2026-09-29 a message to a Claude seat showed
`sent` and never reached the session until the harness was restarted.

`scripts/st3-graceful-messaging-eval/run ST3_BIN OUT HARNESS...` runs it once. It starts one fresh
isolated daemon from a copy of `ST3_BIN` and one seat for each named harness: `claude`, `codex`,
`omp`, `pi`, or `opencode`. The seats get no authored prompt; st3 supplies the boot contract. Once
every seat is idle, the runner does what a deploy does. It installs a replacement at the daemon's
binary path with `mv`, then stops the daemon and starts it again from that path. The replacement is
`ST3_BIN` with a marker appended, so it is a different file with different contents; `GM_SWAP_BIN`
names another build instead. As soon as the restarted daemon answers, the runner sends each seat one
message.

A seat passes when all of these hold:

- the message is `read` within ten seconds of being sent, by the graph's own `message.read` time;
- the runtime incarnation is unchanged;
- the driver process has the same PID and now executes the replacement binary;
- every provider process (the Claude or omp TUI, or the Codex TUI, app-server, and watchdog) has
  the same PID;
- no channel process (Claude's `claude-mcp`, or the pi-family channel) still executes the deleted
  binary;
- `st agents show` reports the seat's delivery path as `current`.

The runner must run outside any st seat. The isolated daemon binds a caller to the nearest `ST_AGENT`
among its ancestors, and the runner sends as `person/eval`. From a seat, start it detached, for
example with `setsid -f env -i HOME="$HOME" PATH=/usr/bin:/bin bash scripts/...`. Pass
`XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` through when the host has a user service manager,
so each seat gets its own scope, as it does under a deployed daemon.

The defaults are Claude `haiku`, Codex `gpt-6-luna`, and omp or pi on `openai-codex/gpt-5.6-luna`,
each at low effort where the harness takes one. The ten-second bound measures the delivery path plus
one short model turn, so the eval uses fast models. `GM_MODEL_CLAUDE`, `GM_MODEL_CODEX`,
`GM_MODEL_OMP`, `GM_MODEL_PI`, and `GM_MODEL_OPENCODE` override them.

`OUT/result.json` holds the verdict and one record per seat. `OUT/evidence/` keeps the seat KDL, the
agent cards and process snapshots before and after, each message's graph trace, each native
transcript, the driver log, and the daemon log.
