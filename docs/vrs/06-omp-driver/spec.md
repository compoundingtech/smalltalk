# omp driver specification

This document specifies the omp native driver implementation. It builds on
[requirements.md](./requirements.md) (OMP-R01..R05, OMP-T01..T03) and the
measured surface in
[`2026-08-25-omp-harness-integration.md`](./.experiments/2026-08-25-omp-harness-integration.md).

## Status

Implemented in this change set: the `omp` driver block and expansion, the
`omp-session` wrapper with the hard 18.x gate, the `omp-channel.ts` asset (type-checked and
smoke-driven under `checks.pi-extension-types`), and the shared channel loop's blocked-frame
parsing. The driver-level decisions are recorded in
[decision 0007](../.decisions/0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate.md).
Open questions are tracked in [open-questions.md](./open-questions.md).

## Overview

```text
agent spec                    expansion (pure)              runtime
──────────────                ──────────────────            ─────────────────────────────
driver omp {                 ┌──────────────┐   task argv: st2 driver omp-session
  model    "…"               │ expand_omp    │             --identity … --runtime-id …
  thinking "high"     ─────► │  in driver.rs │──────────►  -- <omp --model … -e <set>/omp-channel.ts>
  prompt   "…"               └──────────────┘                        │
}                                                          wrapper: presence lease,
                                                           terminal harness-state record
                                                                   │ spawns
                                                           st2 driver omp-channel --identity …
                                                            ▲ newline-JSON frames over stdio
                                                           omp-channel.ts (in-process extension)
```

## Driver block

`OmpDriver { model: Option<String>, thinking: Option<String>, prompt: String }`, KDL
kebab-case, mirroring `PiDriver`. omp has no effort flag; its analog is `--thinking`
(off|minimal|low|medium|high|xhigh|max|auto), so the field is named for what omp exposes.
Expansion prepends `--model` and `--thinking` when set; extra args ride the existing driver
args escape.

## Wrapper (`src/omp_session.rs`)

Shape of `pi_session.rs`:

- Resolves the agent dir, installs the signal handler, claims the observed-state record under
  a freshly minted session token.
- Version gate first: runs `<omp> --version`, parses a strict `MAJOR.MINOR.PATCH` release, and
  admits only a MINOR already measured (`SUPPORTED_OMP_MINORS`); outside that set the launch
  fails loudly with the measured-checks message, per OMP-R05 and decision 0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate. Admission is per
  minor: a patch inside an admitted minor launches without new evidence, and a later *minor*
  stays rejected until the checks are repeated against it. The parse is what keeps "per minor"
  from decaying into "starts with 18" — minors are compared numerically (`18.10` is not `18.1`),
  exactly three components are required, and a pre-release or build-metadata suffix
  (`18.0.9-rc1`) does not parse at all, so it is never admitted as its base release.
  Which token the release is read FROM matters as much as how it parses: an `omp/<release>`
  token is omp naming itself and the first one decides outright, and if what it named cannot be
  parsed the gate refuses rather than reading some other token in the banner. Otherwise `omp/18.1.0-rc1
  18.0.9` would launch an unverified provider on the strength of a version omp never claimed —
  which is the shape DQ-OMP-5's update banner could produce. With no own label, every parseable
  release in the banner must agree.
- Injects the channel extension from the verified hook set (`with_channel_extension` shape —
  resolved from this binary's immutable asset, never a catalog-pinned path).
- Applies offline defaults (`PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1`) unless the operator's
  declaration already set them; suppression of the update banner itself is DQ-OMP-5.
- Exports the channel env (`ST2_OMP_CHANNEL_{BIN,CATALOG,IDENTITY,RUNTIME_ID,SESSION,SEQ}`)
  using fresh names — an omp seat must never adopt a stray pi channel env.
- On provider exit writes the terminal observed record; presence decays by staleness as for
  pi (SIGKILL produces no terminal event).

## Channel (`hooks/omp-channel.ts`)

Forked from `pi-channel.ts`; same frame protocol discipline (LF-delimited JSON, hello /
message / delivered / failed / state / context frames, PROTOCOL constant). Differences:

- **Idle and terminal edges:** `agent_start` emits active. On a terminal `agent_end`, poll
  `ctx.isIdle()` every ~100 ms with a bounded window and emit idle at the first true sample.
  Every poll captures a monotonically increasing generation; a newer settle attempt, new
  `agent_start`, structured ask, approval ask, session replacement, shutdown,
  `willContinue:true`, or terminal error advances the generation and retires older polls before
  they can overwrite newer state. `willContinue:true` means omp already scheduled another turn,
  so that event starts no settle poll. No `agent_settled` listener exists.
- **Typed turn result (OMP-R06):** every `agent_end` that is not `willContinue:true` emits one
  `{type:"turn"}` frame. When the latest assistant message has `stopReason:"error"` the frame
  carries `error: { reason, errorId? }` — omp's whitespace-normalized, 240-character-bounded
  `errorMessage` and its own classification bitfield, forwarded raw; otherwise the frame carries
  no error and is the positive proof the provider accepted the credential. This frame REPLACES
  the terminal error's own state frame: the credential edge and the categorical state are one
  observation on two axes, and correlating them across two frames would be a race st2 cannot
  win. `errorStatus` is deliberately not on the wire — three of the four measured 403s are not
  credential rejections, and omp already prefixes the status to the prose.
- **Structured ask axis:** an `ask` `tool_call` with a valid question emits active with
  `blockedOn:"human"`, `ask:"question"`, and the first nonblank question as its bounded reason.
  The process-wide stash retains its `toolCallId`; unrelated `tool_result` events emit nothing,
  and only the matching result clears the ask to the activity proved by `isIdle()`.
- **Approval axis:** `tool_approval_requested` emits active with `blockedOn:"human"`,
  `ask:"permission"`, and the tool name as reason; `tool_approval_resolved` clears it to the
  activity proved by `isIdle()`. Approval frames do not overwrite a tracked structured ask.
- **Pre-compaction edge:** `session_before_compact` emits `{type:"pre_compact"}`. The extension
  carries no durable path and writes no context itself.
- **Session lifecycle:** `session_start` opens the channel and seeds state from `isIdle()`;
  replacement sessions close their named predecessor in `open()`. Upstream defines
  `session_shutdown` without a `reason` field and fires it on process exit, so every such event
  closes the current channel.
- **Restored context:** seeding uses
  `sendMessage({customType:"st2-session-start", …}, {deliverAs:"nextTurn"})`.

## Rust channel process

`st2 driver omp-channel` reuses the pi channel's loop (`pi_channel.rs`) parameterized by the
`ChannelKind` for `"omp"`; the state frame parser accepts the blocked fields. On `pre_compact`, Rust
resolves `<agent>/resources/context/now.md` through the canonical context API. The blank predicate
and atomic replacement execute under the same lock used by every `now.md` writer, so an authored
write cannot land between them. Only `NotFound` or successfully decoded whitespace-only content
permits the recovery stub; nonblank content is preserved, and every other read failure leaves the
entry untouched and publishes a deterministic actionable error state. The ding side gains no omp
adapter (OMP-T03): delivery is channel-only, failing closed when absent.

The `{type:"turn"}` frame lands on two independent records. Categorically, a provider error is
`active` — nothing is running, but a record saying `idle` would read as a healthy yield — with
reason `providerAuth` for the credential class and omp's own bounded prose for every other one; an
ordinary end asserts nothing, because the sampled idle poll still owns that edge. On the
native-driver diagnostic it publishes `providerAuth`/`providerAuthRejected`/`turnResult` under
driver word `omp`, or clears that stage. Each edge uses a fresh publisher, so the on-disk record is
what carries a rejection across a channel restart. `ChannelKind` is what keeps this out of the pi
channel: pi's extension has no classification field to forward, so `diagnostic_driver` is `None`
there and the same loop publishes no credential verdict for it.

## Admission evidence required for a new minor

Per OMP-R05 and decision 0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate, admitting a new omp MINOR requires re-running: extension-load
probe, lifecycle event inventory, idle-edge sampling, approval-event capture, live delivery
loop — updating the `.experiments/` capture and `SUPPORTED_OMP_MINORS` together. Each probe must
record measured output; a minor that was not measured is not admitted, so the admitted set is
asserted literally in the wrapper's tests.

Patches inside an admitted minor cost nothing: omp releases near-daily, and gating them blocked
the fleet on changes the capture already covered — 18.0.10 shipped within hours of 18.0.9 being
admitted. The evidence a minor is admitted on is a measurement of *some* release in that minor,
and the risk accepted is that a patch could move delivery-critical behavior within it. That has
been observed once and absorbed: between 18.0.3 and 18.0.9 the idle edge moved from ~251 ms to
~25 ms, which the bounded polling rule (OMP-R03) handles without change.

Captures, per measured release:

- 18.0.3 — [`2026-08-25-omp-harness-integration.md`](./.experiments/2026-08-25-omp-harness-integration.md)
  (also the original port evidence).
- 18.0.9 — [`2026-08-28-omp-18-0-9-admission.md`](./.experiments/2026-08-28-omp-18-0-9-admission.md).

Behavioral captures that are not minor admissions:

- provider-credential classification (18.1.7) —
  [`2026-09-05-omp-provider-credential-rejection.md`](./.experiments/2026-09-05-omp-provider-credential-rejection.md),
  the measured `errorId` table OMP-R06's verdict is derived from.
