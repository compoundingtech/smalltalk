# Boot canaries

A seat that cannot start must never ship. `run` boots seats of one harness against a real st3
daemon and a stand-in provider, and checks that each seat reaches a claimed step and reads an st
message.

    run BINARY EVIDENCE_DIR HARNESS SCENARIO [--bound SECONDS] [--scratch DIR] [--replacement-binary BINARY]

`HARNESS` is `claude`, `codex`, `pi`, `omp` or `opencode`. `SCENARIO` is `fresh`, `restart`,
`daemon-restart`, `reexec`, `concurrent`, `suspend` or (Codex only) `import`; `run --help` says
what each does. Each stand-in keeps its sessions the way its harness does, so `suspend` proves that a resumed seat comes
back on the session it suspended on, holds hour-old mail, and consumes recent never-offered
mail exactly once. The Rust wrapper
`crates/st3/tests/boot_canaries.rs` runs every pair as its own test in the required Linux gate.

The boot, no-st2 and subagent fixtures put their private bin directory first, followed by
inherited `PATH` entries that neither contain an `st2` executable nor name an `st2` path.
Provide Bash, Python, `env`, Git, curl and `pty` on the inherited path, plus Node for pi/omp.
Login shells and stand-in interpreters use discovered absolute executables; conventional
system bin directories are not required, so these fixtures also run in Nix sandboxes.

The OMP `when-idle` regression uses a separate runner:

    ../st3-rollout-binding-canary/run BINARY EVIDENCE_DIR omp when-idle [--omp EXECUTABLE]

It uses raw OMP from `--omp`, `OMP_BIN`, or `PATH`, and uses the extension-host fixture only
when OMP is absent. The verdict identifies which provider ran. No model prompts are sent.
An owned-set publication must reach rollout phase `running`, with a new incarnation bound to
the original native UUID and the same transcript inode. With real OMP, an inspection extension
also verifies the historical conversation loaded on both starts. The Rust test runs this in
the Linux gate with the same zero-retry policy as the other boot canaries.

The Codex-only `utf8` scenario starts and ends the bounded transcript window inside a euro sign,
withholds live receipts so transcript recovery must release the next message, then forces a
discovery I/O error. A live status update and receipt must still reach the same seat, and repeated
warnings must stay rate-limited in its private `driver.log`, outside the PTY.

Claude's `channel-missing` and `channel-uninitialized` scenarios restart with healthy hooks
but no usable channel. They require `claude-channel-unattached` with a blocked seat within
45 seconds, a visible hold for early mail, and recovery with exactly one native offer and one
staged/delivered/read receipt. Two further restarts must deliver fresh startup mail once without
replaying recovered mail. The missing-channel seat must restart automatically, continuing its
native session. The uninitialized channel attaches during the recheck window and must keep its
incarnation. Automatic recovery waits 10, 20 and 40 seconds after detection before each of three
restarts; a channel still missing after the third replacement parks the seat with a visible
failure and holds mail until an operator restart or a new declaration. The fault controls live
only in each isolated workspace.
The `channel-parked` scenario keeps the channel missing through all three replacements and
restarts the daemon after parking; neither a fourth attempt nor a mail offer may appear.

Every fresh scenario also checks that the native driver creates no `catalog.kdl`, `agent.kdl`,
or polled `harness-state`, `harness-context`, and `harness-timeline` records, and that each seat
has its st-owned observation outbox.
Re-execution must preserve the provider processes as well as the seat incarnation. To check a
rolling upgrade, give `run` the predecessor binary and use `--replacement-binary` for the new
binary with the `reexec` scenario. That mode allows predecessor catalogs, but checks their bytes
remain unchanged after adoption and a new message reaches the same provider session.

The Codex `import` scenario imports a saved rollout from a symlinked home while the daemon
environment names a stale copy in another home. Its first launch and explicit restart must bind
the exact selected ID, claim work and read mail using the pinned originating `CODEX_HOME`.

## What is real and what is not

Real: the st3 daemon, `pty` sessions, the driver and channel processes, the extension files st
loads into pi and omp, the mailbox, the work graph, and the `st` CLI the stand-in calls.

Stand-in: the provider only. Each `stub-*` speaks the wire contract of its harness and does what a
model does when woken: read the message, run the command the wake gives, exactly as written (`stubmodel.py`).
No login, no model, no tokens. The stand-ins answer instantly where real providers take seconds,
so races the real ones hide are hit every time.

| harness | stand-in | contract it speaks |
| --- | --- | --- |
| claude | `stub-claude.py` | hooks, status line, MCP channel notification, transcript |
| codex | `stub-codex.py` | `app-server` WebSocket JSON on a Unix socket, plus the `--remote` TUI |
| pi, omp | `stub-pi-family.mjs` | the real extension (`-e FILE`) in a minimal host (Node 24) |
| opencode | `stub-opencode.py` | local HTTP and SSE server with Basic auth |

## When a canary fails

The evidence directory holds `result.json` (the verdict and the seat's last state), `daemon.log`,
`terminal-*.txt` (the seat's screen), `trace-*.jsonl` (its claims), `receipts-*.jsonl` (what the
stand-in saw and did), and `observation-outboxes.json` (pending event sequences and driver
diagnostics). Predecessor adoption can also leave legacy harness records. Read the trace first: it shows which side
wrote what, in what order. Fix the product on main; do not slow a stand-in to hide a race. The
canaries found these when they were written: Codex ending its session before the daemon's first
mailbox replay, Codex publishing its predecessor's `ended` record as the new incarnation's, and a
replacement driver binding while the daemon's last observation was an exit with no incarnation.

## Adding a harness or scenario

Add the stand-in, install it in `install_stub`, and add the harness to `HARNESSES` and the `harness!`
list in the Rust wrapper. A scenario is a function of the daemon and the seats in `scenario()`; it
ends by calling `Seats.prove`, which is the one definition of "booted".
