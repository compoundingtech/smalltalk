# omp driver requirements

## Context

This subsystem specifies the omp native driver: the typed `omp` driver block, its expansion,
the session wrapper, and the injected-extension channel that delivers st2 messages and
publishes observed harness state. It refines two existing subsystems rather than replacing
them: typed driver blocks are defined by [`02-agent-spec`](../02-agent-spec/), and the
observed-harness-state record this driver writes is defined by
[`05-harness-state`](../05-harness-state/). The delivery principle is
[decision 0005](../.decisions/0005-pi-delivers-natively-through-an-injected-extension.md)
(native extension delivery, never DING screen-scraping), applied to a fifth harness; the
driver-level choices are recorded in
[decision 0007](../.decisions/0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate.md).
Where this file and those disagree, they win and this file is wrong.

omp is an earendil-works/pi-family harness: it loads pi-style TypeScript extensions and reads
pi's env fallbacks. Measured integration evidence:
[`2026-08-25-omp-harness-integration.md`](./.experiments/2026-08-25-omp-harness-integration.md).

## Assumptions

- **OMP-A01 omp keeps its pi-compatible extension surface:** the three calls the channel uses
  (`sendUserMessage`, `sendMessage`, `on`) keep their names, signatures, and event-name
  vocabulary. The hard version gate (OMP-R05) exists because this assumption is versioned,
  not guaranteed.
- **OMP-A02 Unattended seats run with approvals resolved:** like every other maintained
  driver, a declared seat is expected to launch with an approval mode that does not block on
  a human; the blocked-on-human axis exists to report the cases where it blocks anyway.
- **OMP-A03 Trusted writers:** as in OHS-A03 — the wrapper and its channel subprocess are
  trusted writers of catalog state under the trusted-fleet model.

## Requirements

- **OMP-R01 Full native driver parity.** A declared `driver omp` seat gets: pure KDL
  expansion from the typed block, a wrapper owning the presence lease and the terminal
  observed-state record, native inbox delivery through the injected channel, and live
  active/idle/blocked observations. No DING screen adapter is required for correctness;
  delivery never depends on reading the pane.
- **OMP-R02 Observed axes.** The driver publishes `active` on turn start, `idle` on the
  settled edge (`agent_end` then `isIdle()` true), and `blockedOn: human` between
  `tool_approval_requested` and `tool_approval_resolved`, whose `approved` payload is the
  exit edge back to the prior activity. Lost evidence stops the heartbeat per the generic
  record rules.
- **OMP-R03 Idle edge without `agent_settled`.** Because omp lacks pi's `agent_settled`
  event, the channel derives idle by bounded polling of `ctx.isIdle()` after `agent_end`; it
  must not emit idle at `agent_end` itself (measured still false there) nor wait for an
  event that never fires.
- **OMP-R04 Own channel asset.** omp gets its own `omp-channel.ts`, forked from the pi
  channel's frame discipline, not a shared file parameterized at runtime — the idle-edge and
  approval logic differ, and a shared file would make each harness's correctness depend on
  the other's branch.
- **OMP-R05 Hard version gate.** The wrapper refuses to launch outside the verified range
  (initially omp 18.x) using the codex/opencode admission convention: a later minor stays
  rejected until the delivery-critical checks (event names, idle-edge behavior, approval
  events) are repeated against it.
- **OMP-R06 Provider credential rejection is classified, not guessed.** A turn that ended on a
  provider error carries omp's own `errorId` classification bitfield to st2 unchanged; st2, not
  the asset, decides whether it names a rejected credential, and publishes the shared
  `providerAuth`/`providerAuthRejected` boundary (OHS-R16) when it does. The verdict is
  `AuthFailed` set with `UsageLimit`, `AccountPolicy`, and `Transient` clear, on a value whose
  `Class` bit proves it is a classification and not a bare HTTP status: omp sets `AuthFailed`
  from prose containing `401`, `403`, or `forbidden` as readily as from a status, so that flag
  alone would report an exhausted allowance as a refused credential. A turn omp will retry
  (`willContinue`) claims neither edge, and only a turn that reached its ordinary end clears a
  standing rejection. st2 holds no credential knowledge and no remedy beyond the shared generic
  repair text.

## Acceptable Tradeoffs

- **OMP-T01 Deny-path window.** On `tool_approval_resolved { approved: false }` the channel
  clears the blocked state immediately; whether omp ends the turn or continues (Claude's deny
  path ends it eventlessly) is unmeasured until DQ-OMP-1's capture. v1 accepts a possibly
  brief misprojection there.
- **OMP-T02 Ask axis coarse-grained.** Every approval projects to `ask: permission` in v1;
  distinguishing question-form prompts (Claude's `tool_name` rule) awaits DQ-OMP-2.
- **OMP-T03 No ding composer adapter.** When the channel is dead, delivery fails closed and
  presence decays; no screen-scrape fallback is built for omp in v1, matching the pi and
  opencode drivers.

## Delimiters

Not this subsystem: the harness-state envelope and freshness rules
([`05-harness-state`](../05-harness-state/)); driver-block KDL grammar
([`02-agent-spec`](../02-agent-spec/)); DING transports
([`01-ding`](../01-ding/)). Open questions live in
[open-questions.md](./open-questions.md).
