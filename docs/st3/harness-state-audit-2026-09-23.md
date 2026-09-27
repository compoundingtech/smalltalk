# Harness state and native-session audit — 2026-09-23

This audit answers the fleet questions raised while Codex message delivery was failing. It compares
st3 with Herdr's current source and public contracts. The desired end state is evidence-first:
capture the native evidence, preserve its provenance and sequence, then derive one conservative
normalized state. Missing or stale evidence must become `indeterminate`, never guessed `idle`.

## Conclusions

- st3 has five interactive harness drivers: Claude, Codex, Pi, OMP, and OpenCode. Typecase does
  not need remote control and this work does not enable it.
- `st3 import ls/show` is the harness-neutral local-session inventory. It now discovers saved and
  running sessions for all five drivers. `st3 import run` performs a revision-fenced, exact-process
  takeover, declares one durable ownerless agent seat, records the native session identity in the
  graph, and resumes the same native session. It does not manufacture a standing mission.
- Reconstructing an earlier session is therefore a product capability, not an accidental Claude
  feature. Transcript normalization is driver-specific, but its client timeline is common.
- Seats separated from mission work fix the ownership half of stuck work: work survives a harness
  incarnation, claims expire, and another seat can claim it. They do not make delivery infallible.
  The live COS reply that remained `sent` until an explicit inbox read proves that. On-demand
  Codex `thread/read` discovery and the compaction delivery eval close that known Codex gap. Two
  2026-09-23 `pty-rust` runs also remained ready across six `sent` wake messages while the Claude
  seat reported `running` and `ready`; this is direct evidence that harness readiness and delivery
  health must remain separate axes.
- The nine currently running fleet agents are still declared through the old standing-mission
  pattern. Every one has an `owner_run_id` under `mission-run/fleet/*/standing`; none is yet a
  top-level ownerless seat. New imports use the new seat model, but the existing nine still need a
  separately authorized migration.

## Current trust model

st3 does not infer `idle` from a quiet terminal. Each driver owns an atomic harness-state record.
The record has an ownership incarnation and monotonically increasing ownership and transition
sequences. A reader cross-checks local process liveness, rejects future-skewed or malformed data,
and turns stale or missing proof into an explicit `indeterminate` reason. The graph projection now
exports:

- normalized state, driver, transport, blocked-on-human axis, ask kind, composer state, reason,
  exit, and runtime incarnation;
- state-entry and last-observed timestamps;
- driver ownership sequence, transition sequence, and evidence incarnation;
- context occupancy, model, token/cost/rate-limit data, compaction count, last compaction time, and
  manual/automatic/unknown trigger;
- message `sent -> staged -> delivered -> read -> closed` lifecycle, where `staged` means the local
  driver transport durably accepted the message before native harness acknowledgement.

That is strong evidence, but it is not magic. A provider can change an event contract, a plugin can
fail to load, or an observer can miss an event. Trust therefore comes from native-source tests,
staleness, provenance, incarnation fencing, and black-box evals—not from the word `idle` alone.

## Driver coverage

| Driver | Lifecycle authority | Delivery authority | Native session import | Confidence and remaining risk |
| --- | --- | --- | --- | --- |
| Claude | channel/MCP and official hooks | native channel | ID + JSONL | Strong semantic events; test hook/session replacement and compaction identity changes. |
| Codex | app-server notifications plus transcript fallback | app-server `thread/read`, then `turn/steer` or `turn/start` | ID + JSONL | Strongest protocol, but active-turn discovery was the live gap. Never depend only on a cached turn ID. |
| Pi | channel extension events | native channel | ID/path + JSONL | Strong semantic plugin; preserve session reload and continuation ordering. |
| OMP | Pi-family channel extension events | native channel | ID/path + JSONL; canonical `--resume=<value>` | Strong semantic plugin; preserve Ask/approval and internal reload behavior. |
| OpenCode | native SSE/plugin session machine | native control/session path | ID + SQLite | Strong semantic events; parent/child permission and session-selection changes need black-box coverage. |

The raw-PTY import eval exercises every row by creating a session outside st3, stopping it,
restarting it with its native reference, importing it, proving the exact predecessor PID exited,
and proving the replacement is an ownerless st3 seat resuming that reference.

## Herdr comparison

Herdr is an appropriate reference because it externalizes both normalized state and why it believes
that state. Its integration contract accepts sequenced lifecycle and session reports. Its agent API
exposes lifecycle state, native session source/kind/value, terminal titles, declared and foreground
working directories, launch readiness, state-change sequence, completion sequence, and revision.
Its detection explanation includes the manifest source/version, matched rule, all evaluated rules,
fallback/skip reasons, warnings, and update errors. Its snapshot persists the native session and
launch argv. See the upstream [agent API](https://github.com/herdrdev/herdr/blob/master/src/api/schema/agents.rs),
[detection explanation](https://github.com/herdrdev/herdr/blob/master/src/detect/manifest.rs),
[native resume plans](https://github.com/herdrdev/herdr/blob/master/src/agent_resume.rs), and
[integration documentation](https://herdr.dev/docs/integrations/).

st3 is already stronger in several places: native app-server delivery for Codex, durable graph
claims, incarnation-fenced runtime ownership, explicit human-block/ask/composer axes, context and
rate-limit telemetry, message receipts, and normalized conversation/tool timelines. Herdr is
currently stronger in public explanation and some presentation/session metadata.

| Herdr evidence | st3 disposition |
| --- | --- |
| Native session source, agent, ID/path kind and value | Persisted immediately for imports as `harness.session-file`; add live publication for every managed driver session. |
| State-change and completion sequence | Ownership and transition sequences now reach `harness.observed`; add a view-relative completion/seen concept only if the UI needs `done` distinct from `idle`. |
| Detection source, rule, fallback, skipped reason, warning | st3 exports driver/transport/reason and raw sequence identity; add a structured `harness.explanation` projection rather than embedding screen text. |
| Foreground cwd and terminal title | Capture foreground cwd separately from declared workspace. Capture stripped title only when useful; raw title is untrusted presentation data. |
| Launch pending and interactive ready | Runtime starting/readiness already exists; expose both directly in client agent resources instead of forcing inference. |
| Launch argv in recovery snapshot | Persist a redacted argv/plan digest and canonical resume plan. Never persist credential-bearing argv verbatim. |
| Arbitrary state labels and metadata tokens | Preserve as namespaced diagnostic metadata; never let them override normalized lifecycle semantics. |
| Terminal ANSI history | Do not copy by default. It is large and potentially sensitive; terminal/log access already exists and any snapshot must be explicit opt-in. |

## Verification contract

The following tests/evals are required before calling the gap closed:

1. Unit protocol tests recover the one `inProgress` Codex turn from `thread/read`, reject ambiguous
   snapshots, steer only that turn, and fall back safely after restart/transcript recovery.
2. A black-box Codex eval sends while working, requires `staged/delivered/read`, manually triggers
   `/compact`, observes the compaction edge in graph usage, then proves post-compaction delivery.
   `mission-run/live-20260923113533` passed this contract: all three messages have staged,
   delivered, and read claims; the sole terminal input is classified `context-compaction`; and the
   driver published one `manual` compaction before accepting the final message. The preserved
   [live report](../../evals/st3/codex-compaction-message-delivery/reports/2026-09-23-live-20260923113533.md)
   contains the exact subjects and store indices.
3. A black-box import eval launches raw PTYs—not st3 terminals—for all five drivers and proves
   revision fencing, exact process termination, durable-seat declaration, and native resume.
4. Cross-harness wake coverage continues to exercise Claude, Codex, Pi, and OMP. Add OpenCode to
   that matrix when an account-backed live environment is continuously available.
5. Status fixtures must test stale evidence, dead-session downgrade, ownership takeover, duplicate
   and out-of-order events, internal session replacement, approval/question waits, completion,
   crash, provider capacity, and driver/plugin absence for every driver.

## Follow-up work

The current implementation closes the observed Codex turn-ID/compaction gap and adds import parity.
Perfect externalization still requires graph-backed follow-up for live native-session identity on
all managed seats, structured explanation/provenance, foreground cwd, safe terminal-title metadata,
and the remaining driver scenario matrix. Those are feature gaps, not reasons to report a guessed
state: until evidence exists, the correct state is `indeterminate` with a reason.
