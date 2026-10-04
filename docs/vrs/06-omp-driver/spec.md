# omp driver specification

This document specifies the omp native driver implementation. It builds on
[requirements.md](./requirements.md) (OMP-R01..R05, OMP-T01..T03) and the
measured surface in
[`2026-08-25-omp-harness-integration.md`](./.experiments/2026-08-25-omp-harness-integration.md).

## Status

Implemented in this change set: the `omp` driver block and expansion, the
`omp-session` wrapper with measured exact-build admission, the `omp-channel.ts` asset (type-checked and
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

## Wrapper (`crates/st-drivers/src/omp_session.rs`)

Shape of `pi_session.rs`:

- Resolves the agent dir, installs the signal handler, claims the observed-state record under
  a freshly minted session token.
- For an ordinary launch, runs `<omp> --version` first. For a residency wake, validates the
  provider checkpoint and rejects every authored session selector before starting even that
  diagnostic child. A missing, corrupt, foreign, stale, or changed binding fails closed. The
  validated native session ID is injected as `omp --resume <id>`; there is no fresh-session
  fallback. Admission parses the producer's own unambiguous strict `MAJOR.MINOR.PATCH`
  release, then measures its exact executable/installation/interpreter identity and the shipped
  extension bytes. An `omp/<release>` label decides; an unreadable own label cannot be rescued
  by another banner token. Prereleases and build metadata are not silently admitted as their
  base release. There is no minor allowlist.
  The first unseen identity runs the five checks below in a disposable RPC session with empty
  HOME/XDG/credential/workspace/PTY roots and a local model fixture. Successful and failed
  results are atomically stored under the harness state root's `harness-admission/` directory,
  including probe and adapter implementation identity. Concurrent starts share a bounded lock.
  A failure names the check and refuses launch before ownership; st3 projects that durable
  diagnostic into the current incarnation's harness state and doctor. Removing a refused record
  after repairing the producer/adapter contract requests another measurement.
  A person can explicitly override admission for an exact installed build as described below;
  this is retained as an exception, never as passing measured evidence.
- Injects the channel extension from the verified hook set (`with_channel_extension` shape —
  resolved from this binary's immutable asset, never a catalog-pinned path).
- Applies offline defaults (`PI_OFFLINE=1`, `PI_SKIP_VERSION_CHECK=1`) unless the operator's
  declaration already set them; suppression of the update banner itself is DQ-OMP-5.
- Exports the channel env (`ST2_OMP_CHANNEL_{BIN,CATALOG,IDENTITY,RUNTIME_ID,SESSION,SEQ}`).
  A residency wake also exports `EXPECTED_NATIVE_SESSION` and `RESUME_GENERATION`; ordinary
  launches explicitly remove both fence variables from the inherited provider environment.
  Fresh names ensure that an omp seat never adopts a stray pi channel env.
- On provider exit writes the terminal observed record; presence decays by staleness as for
  pi (SIGKILL produces no terminal event).

## Channel (`hooks/omp-channel.ts`)

Forked from `pi-channel.ts`; same frame protocol discipline (LF-delimited JSON, hello /
message / delivered / failed / state / context frames, PROTOCOL constant). Differences:

- **Idle and terminal edges:** `agent_start` emits active. On a terminal `agent_end`, poll
  `ctx.isIdle()` every ~100 ms until positive proof or superseding ownership/activity, and emit
  idle at the first true sample only when no human ask or approval is pending.
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
- **Session lifecycle and native binding:** `session_start` requires
  `ctx.sessionManager.getSessionId()`, opens the channel, and sends that native OMP session ID
  before accepting Rust's hello. After the hello passes the protocol gate, the extension returns
  a matching ready frame. Only this two-way exchange makes the binding ready. The mandatory
  generation and expected-ID fence applies to the first restored session of a cold launch; an
  explicit later in-process session switch becomes the current binding instead of inheriting the
  old fence. Replacement sessions close their named predecessor in `open()`. Upstream defines
  `session_shutdown` without a `reason` field and fires it on process exit, so every such event
  closes the current channel.
- **Subagent sessions:** omp loads the extension into every in-process subagent (the `task`
  tool, eval `agent()`, `/tan` clones), and each copy shares the process-wide stash. Every
  handler ignores an event whose `ctx.agent.kind` is `"sub"`, so a subagent opens no channel,
  closes none, emits no frame, and never receives the seat's mail; the channel stays bound to the
  top-level session. omp exposes `ctx.agent` from 18.3.2; an earlier build reads as top-level.
- **Restored context:** seeding uses
  `sendMessage({customType:"st2-session-start", …}, {deliverAs:"nextTurn"})`.

## Seat label authority

`desired.display_name` (KDL `name`) is the authority for the human label. Its stored declaration
contains the canonical KDL name child and the normalized member's `display_name`; rename keeps
these two representations identical and preserves the original declaring actor.
`st3 agents rename <subject> <label>` and `st3 agents rename <subject> --clear` publish only
this presentation field through the durable desired-state log, without restarting the harness.
In free mode, any person or agent may rename any seat, but a bound harness must act as itself.
Rename requires a non-empty label and refuses a seat whose stored launch this build cannot read,
since republishing it would erase that launch.
Clearing restores the subject without the `agent/` prefix, which the Agent API uses as the
effective label. A revision that differs from a predecessor only in the label (for a merge of
concurrent revisions, from any one of its predecessors) keeps that predecessor's launch revision:
launch records, restart budgets, restart-window resets, and crash-loop holds all key on the launch
revision, so a rename never restarts a finished, exhausted, or parked seat. A seat is relaunched
for its declaration only when its latest launch is not in that lineage.

## Rust channel process

`st2 driver omp-channel` reuses the pi channel's loop (`pi_channel.rs`) parameterized by the
`ChannelKind` for `"omp"`; the state frame parser accepts the blocked fields. Before publishing
hello, the OMP endpoint stores a separate not-ready candidate and validates its wrapper
incarnation, residency generation, and required native ID. The checkpoint binding remains
authoritative and retryable if the channel exits before the exchange completes. Only a matching
ready frame promotes the candidate to the authoritative binding. Residency readiness therefore
proves the new wrapper incarnation, exact native session, exact generation, and completed
two-way channel handshake; a binding from before the checkpoint remains starting rather than
becoming a false positive.

On `pre_compact`, Rust resolves `<agent>/resources/context/now.md` through the canonical context
API. The blank predicate and atomic replacement execute under the same lock used by every
`now.md` writer, so an authored write cannot land between them. Only `NotFound` or successfully
decoded whitespace-only content permits the recovery stub; nonblank content is preserved, and
every other read failure leaves the entry untouched and publishes a deterministic actionable
error state. The ding side gains no omp adapter (OMP-T03): delivery is channel-only, failing
closed when absent.

The `{type:"turn"}` frame lands on two independent records. Categorically, a provider error is
`active` — nothing is running, but a record saying `idle` would read as a healthy yield — with
reason `providerAuth` for the credential class and omp's own bounded prose for every other one; an
ordinary end asserts nothing, because the sampled idle poll still owns that edge. On the
native-driver diagnostic it publishes `providerAuth`/`providerAuthRejected`/`turnResult` under
driver word `omp`, or clears that stage. Each edge uses a fresh publisher, so the on-disk record is
what carries a rejection across a channel restart. `ChannelKind` is what keeps this out of the pi
channel: pi's extension has no classification field to forward, so `diagnostic_driver` is `None`
there and the same loop publishes no credential verdict for it.

## Automatic admission evidence for an exact installed build

OMP-R05 and revised decision `0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate`
require all five checks before admitting an unseen identity:

1. Load the shipped extension and companion recorder through the installed producer. Require
   the extension API calls and a real native channel `ready` binding.
2. Observe `session_start`, `agent_start`, `turn_start`, `message_start`, `message_end`,
   `turn_end` and terminal `agent_end` in the disposable session.
3. Observe a positive `ctx.isIdle()` sample after terminal `agent_end`, plus the shipped
   channel's active-to-idle transition.
4. Correlate `tool_approval_requested` and `tool_approval_resolved` by session, tool name and
   tool-call ID. The local model requests an empty fixture tool; the RPC peer denies only
   that fixture approval. Require the shipped channel's human/permission observation.
5. Send a fresh nonce through the shipped channel, require exactly one transport acknowledgement,
   and prove consumption with both the fixture model's native prompt and tool continuation
   and the harness's completed user/assistant messages containing the nonce.

A passed exact build never admits a different patch merely because its minor matches.
Executable, interpreter, npm installation, extension or probe replacement triggers remeasurement.
All subprocesses use disposable roots and are reaped by process group on success, refusal,
timeout or malformed evidence. Version probing, model requests and cache locking are bounded.
The startup probe does not certify context arithmetic, provider/model matrices, pricing,
interactive UI modes or every steer/modal case; their existing measured evidence stays separate.

### Rejection and a person's explicit exception

Admission runs on a new provider launch, including restart and residency resume. It does not
retroactively stop an already-running provider, and driver re-execution that adopts the existing
provider does not repeat admission. A refused omp build does not start a new provider or claim
the live session. There is no rollback to an older executable. OpenCode starts its installed
server with native delivery disabled; a waiting message remains queued. Both policies name the
failed boundary in the current incarnation and `st doctor`.

If a person's own machine cannot run the isolated probe, they may allow that exact installed
build from their terminal, outside an agent seat:

```sh
st admission override omp --binary /path/to/omp --reason 'Probe cannot run in this offline installation'
```

Use `opencode` for OpenCode. `--binary` must identify the executable the affected seat uses;
omitting it selects the harness on that terminal's PATH. `--state-dir` selects a nondefault
daemon state directory; otherwise the command uses the person's local st configuration.
Run the command on the host and as the operating-system user that owns the seats, then restart
only the affected seat. The command works without a daemon, probe child or model credentials.
It records the reason, time and exact executable/interpreter/installation and shipped-extension
identity in a separate user-local `*.override.json` record. It bypasses even scratch `--version`;
it neither replaces failed measurements nor certifies the contract. The driver logs that a
person's exception is active and does not label its support as measured passing. An executable,
dependency, interpreter or shipped-extension replacement needs a new exception. Other builds
retain normal measured admission. The exception persists across st updates with the same
producer and extension. Revocation restores retained measured results on the next launch:

```sh
st admission revoke omp --binary /path/to/omp
```

The st fixture requires loopback sockets and writable temporary/state directories. It sends its
model/API requests only to loopback, uses dummy fixture keys, clears inherited credentials,
disables update/model/plugin fetches where the producer supports those switches, and adds no
separate Bun/Node/Python runtime requirement. The installed producer must still have its own
normal interpreter/dependencies; omp's fixture channel uses POSIX `sh`. A producer may attempt
to bootstrap a missing plugin/package in an empty scratch cache; such an offline failure is a
refusal, not evidence of broken live delivery. The exception permits normal launch in that case.
It cannot supply a missing runtime or repair a genuinely incompatible producer/extension.

The 2026-10-02 deterministic installed-producer captures are retained under
[`crates/st-drivers/tests/fixtures/harness-admission/`](../../../crates/st-drivers/tests/fixtures/harness-admission/):
omp 18.1.22 (all five checks, denied empty tool) and OpenCode 1.18.34 (API/SSE/permission and
completed consumption, approved harmless `printf`). Tests replay these measured captures and
run fake unseen exact versions through the production admission path, with one failure per check.
The generation- and channel-fenced sampling rule (OMP-R03) handles slow final unwind:
sampling continues until positive idle proof or a superseding event, and never publishes
idle while a human ask or approval is pending.

Captures, per measured release:

- 18.0.3 — [`2026-08-25-omp-harness-integration.md`](./.experiments/2026-08-25-omp-harness-integration.md)
  (also the original port evidence).
- 18.0.9 — [`2026-08-28-omp-18-0-9-admission.md`](./.experiments/2026-08-28-omp-18-0-9-admission.md).
- 18.1.2 — [`2026-09-02-omp-18-1-2-admission.md`](./.experiments/2026-09-02-omp-18-1-2-admission.md).
- 18.3.0 — [`2026-09-24-omp-18-3-0-admission.md`](./.experiments/2026-09-24-omp-18-3-0-admission.md).
- 18.4.2 — [`2026-09-29-omp-18-4-2-admission.md`](./.experiments/2026-09-29-omp-18-4-2-admission.md).

Historical captures are evidence for their exact measured builds; automatic admission decides
the currently installed identity independently of those release lists.

Behavioral captures outside the automatic admission contract:

- provider-credential classification (18.1.7) —
  [`2026-09-05-omp-provider-credential-rejection.md`](./.experiments/2026-09-05-omp-provider-credential-rejection.md),
  the measured `errorId` table OMP-R06's verdict is derived from.
