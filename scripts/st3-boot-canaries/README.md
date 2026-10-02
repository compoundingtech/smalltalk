# Boot canaries

A seat that cannot start must never ship. `run` boots seats of one harness against a real st3
daemon and a stand-in provider, and checks that each seat reaches a claimed step and reads an st
message.

    run BINARY EVIDENCE_DIR HARNESS SCENARIO [--bound SECONDS] [--scratch DIR] [--replacement-binary BINARY]

`HARNESS` is `claude`, `codex`, `pi`, `omp` or `opencode`. `SCENARIO` is `fresh`, `restart`,
`daemon-restart`, `reexec` or `concurrent`; `run --help` says what each does. The Rust wrapper
`crates/st3/tests/boot_canaries.rs` runs every pair as its own test in the required Linux gate.

Every scenario also checks that the native driver creates no `catalog.kdl` or `agent.kdl`.
Re-execution must preserve the provider processes as well as the seat incarnation. To check a
rolling upgrade, give `run` the predecessor binary and use `--replacement-binary` for the new
binary with the `reexec` scenario. That mode allows predecessor catalogs, but checks their bytes
remain unchanged after adoption and a new message reaches the same provider session.

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
stand-in saw and did) and the harness-state record. Read the trace first: it shows which side
wrote what, in what order. Fix the product on main; do not slow a stand-in to hide a race. The
canaries found these when they were written: Codex ending its session before the daemon's first
mailbox replay, Codex publishing its predecessor's `ended` record as the new incarnation's, and a
replacement driver binding while the daemon's last observation was an exit with no incarnation.

## Adding a harness or scenario

Add the stand-in, install it in `install_stub`, and add the harness to `HARNESSES` and the `harness!`
list in the Rust wrapper. A scenario is a function of the daemon and the seats in `scenario()`; it
ends by calling `Seats.prove`, which is the one definition of "booted".
