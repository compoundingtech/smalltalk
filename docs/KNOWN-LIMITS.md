# Known limits

Deliberately-accepted gaps, documented next to the code that owns them so the
eventual fixer knows where to look. These are not bugs to file — they are
scoped-out edges with a known direction for a later improvement.

## Liveness: a HUNG agent reads as healthy (only clean death is caught)

**Where:** the status-file heartbeat in `src/commands/ding.ts` (the
`statusRefreshIntervalMs` tick / `runStatusRefreshTick`), consumed as
cross-machine liveness via the synced status-file mtime.

**The contract that works:** an agent is alive iff its `status` file's mtime is
fresh (`now - mtime < T`, reader threshold ~120s). The ding sidecar bumps the
mtime every `LIVENESS_HEARTBEAT_MS` (30s) while the agent is alive. When the
harness process (`exec claude` / `exec codex` — the harness *is* the pty session
process) exits, the ding's exit-when-session-gone watch fires, `stop()` clears
the refresh timer, the touch ceases, the mtime freezes, and any host reads the
agent as dead. **Clean death (process exit / crash / kill) is caught cleanly.**

**The limit:** a **HANG** is not caught. If the harness is wedged (unresponsive)
but its pty session `.pid` still exists, `isSessionAlive` stays true, the ding
keeps running, and it keeps bumping the status mtime — so a stuck agent reads as
**healthy**. This is the operationally interesting case (a harness-freeze), and
it currently produces a false-healthy signal.

**Why it's scoped out here:** the ding can only observe process liveness (is the
session `.pid` there?), not harness *responsiveness*. The heartbeat proves "the
sidecar is running," not "the agent is processing." Distinguishing the two would
change the contract.

**Direction for the fix (separate, later):** have the **agent itself** self-touch
its status file (or a dedicated liveness file) as a side effect of doing real
work — so the freshness proves the harness is *responsive*, not merely that a
sidecar process exists. That makes the heartbeat an agent-liveness signal rather
than a process-liveness one, and catches the hang.
