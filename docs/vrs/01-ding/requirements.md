# DING requirements

## Context

DING is the sidecar that turns an unread inbox message into text in a running
agent's terminal composer. It is the only st2 subsystem that writes into a
surface a human may also be typing into, so its contract is written as a set of
preconditions for pressing Return rather than as a delivery guarantee.

This decomposes [`R05`](../requirements.md) — the ratified floor for inbox
delivery, archive precedence, retries, suppression, and restart recovery — into
the subsystem's own obligations, and inherits the watcher obligations in `R14`
and `R15`. Where this file and the root disagree, the root wins and this file is
wrong.

The realization per maintained harness is specified in
[`01-claude/spec.md`](./01-claude/spec.md) and
[`02-codex/spec.md`](./02-codex/spec.md). The mechanism shared by all harnesses
is in [`spec.md`](./spec.md).

## Assumptions

- **DING-A01 Rendered screens only:** The only available evidence about a
  composer's state is a rendered terminal screen. No maintained harness exposes
  an evented idle signal, so every precondition below is a measured heuristic
  over text. `DQ2` in [`../spec.md`](../spec.md) tracks closing that gap.
- **DING-A02 Cooperative human:** The human sharing a pane is not adversarial.
  A screen that deliberately imitates another harness's composer is a
  correctness concern, not a security boundary, consistent with `A02`.

## Acceptable Tradeoffs

- **DING-T01 Deferral over delivery:** A message that is never delivered is a
  worse outcome than one delivered late, but both are better than text typed
  into a human's draft or a Return pressed on their behalf. Every ambiguous
  case resolves toward deferral.
- **DING-T02 Narrow positive recognition:** Recognizing fewer screen states
  than a harness can render is preferred to recognizing a state loosely. A
  documented unsupported screen is consistent with `T01`; a loosely matched one
  is not.

## Requirements

### Must preserve initial transport and gate every retry

- **DING-R01 Combined initial transport:** A fresh notice uses one bounded PTY
  transaction containing the bracketed paste, the accepted 0.5 second delay,
  and Return. Ownership is recorded before that command starts. Composer
  heuristics do not split or suppress this initial transport.
- **DING-R02 Two adjacent retained-safe retry observations:** A later bare
  Return is permitted only for a transport-owned payload whose exact notice is
  still the complete composer and is classified `RetainedSafe` in two
  immediately adjacent inspections. The final observation is adjacent to the
  Return itself. Any change, block, or uncertainty prevents retry submission.
- **DING-R03 Fail-closed receipt and retry:** After the initial transport, a
  changed composer, a human draft, an active turn, a modal, an unreadable
  screen, an unrecognized harness, and a bounded observation timeout never
  become `Delivered` and receive no retry input. Anything not positively
  understood retains staged ownership. On an inspect-only staged retry, a
  maintained adapter may positively prove that the exact owned payload is no
  longer retained; that proof relinquishes ownership only when an archive
  receipt already removed the notice from the inbox. An unread notice remains
  staged even after positive absence, so it is never pasted again.

### Must classify the surface it will actually type into

- **DING-R04 Live composer, not transcript:** Classification targets the
  composer that will receive the keystrokes — the live composer at the bottom of
  the viewport. Harness-shaped text appearing anywhere in scrollback, including
  a captured or pasted screen from another harness, must not be classified in
  its place. A pane is classified by what it will do with input, not by what its
  transcript resembles.
- **DING-R05 No dependence on transient output:** Harness identification relies
  on durable screen structure. Output present only near session start, such as a
  startup banner, must not be load-bearing, because a screen inspection sees the
  current viewport rather than the session's history.
- **DING-R06 Harness-owned recognition:** Each maintained harness owns the
  vocabulary that identifies its own composer, footer chrome, and active or
  modal states. Shared classification code carries only what is genuinely common
  to every harness, so one harness's rendering cannot silently decide another
  harness's safety gate.

### Must remain bounded and idempotent

- **DING-R07 Staged ownership:** Once a paste command has started, the payload
  is owned by that attempt. Ambiguity about whether the paste landed is resolved
  by inspection only; the same notice is never pasted again until the exact
  staged payload has disappeared or changed.
- **DING-R08 Bounded probing:** Deferred delivery retries on a bounded backoff,
  so an indefinitely busy composer cannot make every inbox poll spawn another
  terminal probe.
- **DING-R09 Presence gate:** Declared `busy` is observable but never suppresses
  delivery; only fresh `dnd` defers it. Delivery may wake a working agent.
- **DING-R10 Positive harness receipt:** `Delivered` requires adapter-provided
  classification that the expected notice text is visible in that harness's
  submitted-prompt or queued-message pattern while its lowest live composer is
  empty or an accepted idle placeholder. PTY command success, generic screen
  change, disappearance alone, and ambiguous pixels are not receipts. A
  maintained adapter that successfully parses the live composer may separately
  prove `NotRetained`; this is never delivery and releases only an already
  archived staged head. Unread, unreadable, unrecognized, and ambiguous attempts
  retain staged ownership and retry by inspection without re-pasting.
- **DING-R11 Readable immutable sender:** For an Agent endpoint, a notice
  resolves the message's canonical sender agent ID to the current bus address
  immediately before rendering. If the address book is absent, unreadable,
  incomplete, ambiguous, or has no current route for that subject, DING falls
  back to the immutable ID and may show the publication-time address snapshot
  only when it is explicitly marked historical. It never renders that snapshot
  as the current sender, because a released address is immediately reusable and
  may already route to a different subject. A principal
  or external endpoint displays its canonical typed address without Agent
  lookup. Display fallback never changes reply targeting, staged ownership, or
  the exact notice used for receipt classification.

## Evidence

Each guarantee above is pinned by a named test in
[`INVARIANTS.md`](../../../INVARIANTS.md) under **Fail-closed observed native
DING**, **Bounded DING PTY probe churn**, **Mutation-only filesystem wakeups**,
and **Agent-declared presence discipline**. A change that keeps those tests
green but violates a requirement here means the invariant set is incomplete, not
that the requirement is satisfied.
