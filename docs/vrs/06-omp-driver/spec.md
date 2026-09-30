# omp driver specification

This document specifies the omp native driver implementation. It builds on
[requirements.md](./requirements.md) (OMP-R01..R05, OMP-T01..T03) and the
measured surface in
[`2026-08-25-omp-harness-integration.md`](./.experiments/2026-08-25-omp-harness-integration.md).

## Status

The typed `harness "omp"` declaration expands to the `st3 driver omp` wrapper. The
wrapper enforces the admitted 18.x minor gate and injects the immutable `omp-channel.ts`
asset, type-checked and smoke-driven under `checks.pi-extension-types`. Driver decisions
are recorded in
[decision 0007](../.decisions/0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate.md).
Open questions are tracked in [open-questions.md](./open-questions.md).

## Overview

```text
agent declaration             expansion (pure)              runtime
─────────────────             ────────────────              ────────────────────────
agent "seat" {                graph.rs                      st3 driver omp
  name "Human label"          agent member ──────────────►   --subject agent/seat
  harness "omp" {                                            -- omp … -e <set>/omp-channel.ts
    model "…"                                                       │
    effort "high"                                            omp (native TUI)
  }                                                                 │ extension spawns
}                                                           st3 driver omp-channel
                                                            --identity seat
                                                            ▲ LF-delimited JSON stdio
                                                           omp-channel.ts
```

## Harness declaration

An st3 seat declares `harness "omp" { model "…"; effort "high"; args "…" }`.
The graph's typed harness parser expands `model` to `--model`, and `effort` to OMP's
`--thinking` (off|minimal|low|medium|high|xhigh|max|auto); extra arguments are preserved.
There is no startup prompt: the seat takes no turn until a person types or a channel
message arrives.

## Wrapper (`src/omp_session.rs`)

Shape of `pi_session.rs`:

- Resolves the agent dir, installs the signal handler, claims the observed-state record under
  a freshly minted session token.
- For an ordinary launch, runs `<omp> --version` first. For a residency wake, validates the
  provider checkpoint and rejects every authored session selector before starting even that
  diagnostic child. A missing, corrupt, foreign, stale, or changed binding fails closed. The
  validated native session ID is injected as `omp --resume <id>`; there is no fresh-session
  fallback. The version gate parses a strict `MAJOR.MINOR.PATCH` release, admits only a MINOR
  already measured (`SUPPORTED_OMP_MINORS`), and fails loudly with the measured-checks message,
  per OMP-R05 and decision
  0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate. Admission is per
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
- Exports the channel env (`ST3_OMP_CHANNEL_{BIN,CATALOG,IDENTITY,RUNTIME_ID,SESSION,SEQ}`).
  A residency wake also exports `EXPECTED_NATIVE_SESSION` and `RESUME_GENERATION`; ordinary
  launches explicitly remove both fence variables from the inherited provider environment.
  Fresh names ensure that an omp seat never adopts a stray pi channel env.
- On provider exit writes the terminal observed record; presence decays by staleness as for
  pi (SIGKILL produces no terminal event).

## Channel (`hooks/omp-channel.ts`)

Forked from `pi-channel.ts`; same frame protocol discipline (LF-delimited JSON, hello /
message / label / delivered / failed / state / context frames, protocol 1). Differences:

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
  observation on two axes, and correlating them across two frames would be a race st3 cannot
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
- **Session lifecycle and native binding:** both `session_start` and `session_switch` require
  `ctx.sessionManager.getSessionId()`, reopen the channel, and send that native OMP session ID
  before accepting Rust's hello. After the hello passes the protocol gate, the extension returns
  a matching ready frame. The extension's generation and expected-ID fence applies to the first
  restored session of a cold launch; an explicit later in-process session switch becomes the
  current binding instead of inheriting the old fence. Replacement sessions close their named
  predecessor in `open()`. Upstream defines
  `session_shutdown` without a `reason` field and fires it on process exit, so every such event
  closes the current channel.
- **Restored context:** seeding uses
  `sendMessage({customType:"st3-session-start", …}, {deliverAs:"nextTurn"})`.

## Seat labels

`desired.display_name` (KDL `name`) is the authority for the human label. Its stored declaration
contains the canonical KDL name child and the normalized member's `display_name`; rename keeps
these two representations identical and preserves the original declaring authority.
`st3 agents rename <subject> <label>` and `st3 agents rename <subject> --clear` publish only
this presentation field through the durable desired-state log, without restarting the harness.
Clearing restores the subject without the `agent/` prefix. The Agent API (and Fractal)
and the running PTY's atomic `displayName` metadata projection use this same effective label.
`ST_AGENT`, `ST3_SUBJECT`, `PTY_SESSION`, and seat/runtime identities remain unchanged.

The cold protocol-1 hello includes `name` with the effective label. The running channel
polls desired state and emits `{"type":"label","name":"…"}` when it changes, including
after a binary re-exec; the last sent label survives re-exec in `PiChannelResume`.
An old resume record without the label receives a label snapshot on the first tick, not
a second context-bearing hello. Both additions are observational: old hooks ignore them.
The OMP hook sets the native session title to the received label followed by
`[${AGENT_PERSONA_SHORT}]` when the launcher supplies a nonempty short code.
Rebinding on native-session start/switch restores the authoritative title after `/new`
or resume. A native `/rename` is only a temporary local title until the next authority
update or rebind, not a mutation of smalltalk desired state.
Managed OMP members export `ST3_OMP_CHANNEL_LABELS=1`, allowing launch-provided status
extensions to relinquish title writes to the channel. Older/direct launch paths retain
their own title writer when that capability is absent.

## Rust channel process

`st3 driver omp-channel` runs the pi-family loop in `crates/st3/src/main.rs`.
It waits for the seat's running incarnation, sends a protocol-1 hello with the effective
label and restored session context, and records categorical harness state and native handoff
receipts through the st3 API. The extension's provider-idle proof gates message delivery. Failed
handoffs remain queued with backoff, and authoritative receipts settle them. The channel
keeps reports across daemon outages and carries its state and partial input frame across
binary re-exec without replaying restored context.

The st3 loop consumes `state`, `delivered`, and `failed` extension frames. `active` projects
to `working`, and `idle` projects to `idle`; optional blocked-on-human details are not projected
by this loop. The hook also emits `session`, `ready`, `context`, `turn`, and `pre_compact`
observations, but this st3 channel does not consume those frames. In particular, this loop
does not publish a durable native-session binding, a pre-compaction context stub, or a typed
provider-auth diagnostic. Those behaviors of the legacy `src/pi_channel.rs` loop must not be
read as st3 guarantees. Labels are independent of those observational frames.

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
- 18.1.2 — [`2026-09-02-omp-18-1-2-admission.md`](./.experiments/2026-09-02-omp-18-1-2-admission.md).
- 18.3.0 — [`2026-09-24-omp-18-3-0-admission.md`](./.experiments/2026-09-24-omp-18-3-0-admission.md).
- 18.4.2 — [`2026-09-29-omp-18-4-2-admission.md`](./.experiments/2026-09-29-omp-18-4-2-admission.md).

The `18.2` minor is not admitted: no release in that minor has been measured.

Behavioral captures that are not minor admissions:

- provider-credential classification (18.1.7) —
  [`2026-09-05-omp-provider-credential-rejection.md`](./.experiments/2026-09-05-omp-provider-credential-rejection.md),
  the measured `errorId` table OMP-R06's verdict is derived from.
