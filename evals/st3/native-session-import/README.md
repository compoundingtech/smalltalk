# Native session import

This black-box eval uses a dedicated `PTY_ROOT` and invokes `pty` directly, outside the st3
runtime. For each supported interactive driver—Codex, Claude, Pi, OMP, and OpenCode—it:

1. starts a fresh raw interactive harness and creates native persisted history;
2. restarts that history in another raw PTY with the native session identifier in the command;
3. requires `st3 import ls/show` to bind the exact process and revision;
4. runs the fenced import, proves the predecessor PID exited, and observes a replacement runtime;
5. proves the replacement is a durable ownerless seat and its managed command resumes the same
   native identifier.

The eval is account-backed and intentionally expensive: it is the end-to-end migration contract,
not a fixture-only parser test. Unit tests separately cover each history format and import KDL.
