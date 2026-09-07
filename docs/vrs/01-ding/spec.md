# DING specification

This document specifies the mechanism shared by every maintained harness. It
builds on [requirements.md](./requirements.md). Per-harness screen grammars are
specified in [`01-claude/spec.md`](./01-claude/spec.md) and
[`02-codex/spec.md`](./02-codex/spec.md).

## Status

Active. A map to the implementation and its evidence, not a replacement for the
tests.

The sender-projection section is the accepted immutable-ID target. The current
implementation remains on bus-identity projection until
[DELTA-003](../.delta/DELTA-003-agent-address-not-implemented.md) closes.

## Composer states

One inspection of a rendered screen, evaluated against one exact expected
notice, yields exactly one state:

| State | Meaning |
| --- | --- |
| `EmptySafe` | A maintained harness is positively idle with an empty composer |
| `ExactSafe` | The exact notice is the complete composer, and the harness is idle |
| `ExactBlocked` | The exact notice is present, but a modal, active turn, or non-idle footer blocks Return |
| `Changed` | A maintained composer holds different text, including a human draft |
| `Ambiguous` | No maintained, unambiguous composer state was proven |

`Ambiguous` is the default for anything not positively understood, per
`DING-R03`. It is not an error state and carries no diagnostic obligation.

## Sender projection

For an Agent endpoint, the durable message carries canonical sender ID plus a
publication-time bus address snapshot. Immediately before constructing the
notice, DING resolves the ID against one coherent current address book. The
displayed sender is the current bus address only when that lookup succeeds.
An absent, unreadable, incomplete, ambiguous, or nonroutable address book
degrades to the immutable ID, which is always displayable, optionally
accompanied by the publication-time snapshot explicitly marked as a historical
address. It never presents that snapshot alone as the current sender: a
released address is immediately reusable, so the saved bytes may already route
to a different subject. Cosmetic lookup never blocks delivery.
A principal or external endpoint displays its canonical typed address
without Agent lookup. Replies retain the canonical endpoint from the message;
rendered text is never reparsed as authority.

## Delivery

```text
record ownership ─► combined transport (paste ─► 0.5s ─► Return) ─► receipt
                         │ command failure or ambiguity                 │
                         └──────────────────────────────────────────────► Staged

receipt ─┬─ Accepted ──────────────────────────────────────────────► Delivered
         └─ RetainedSafe / RetainedBlocked / NotRetained / Unproven ► Staged

staged retry ─► receipt ─┬─ Accepted ──────────────────────────────► Delivered
                         ├─ RetainedSafe ─► final receipt ─┬─ Accepted ─► Delivered
                         │                                 ├─ RetainedSafe ─► Return ─► receipt
                         │                                 └─ other ────────► Staged
                         ├─ NotRetained + archived ────────────────► release head
                         ├─ NotRetained + unread ──────────────────► Staged
                         └─ RetainedBlocked / Unproven ─────────────► Staged
```

Fresh delivery preserves the production transport: one bounded PTY transaction
contains a bracketed paste, a 0.5 second delay, and Return (`DING-R01`).
Ownership is recorded immediately before that transaction. The production path
does not inspect the composer first and does not use the separate staging
helper.

Every failure of that terminal command or of the following receipt observation
resolves to `Staged` (`DING-R07`): the paste and Return may already have reached
the harness, so ownership is retained and retry re-inspects instead of
re-pasting. A staged retry is inspect-only unless two adjacent `RetainedSafe`
observations authorize one bare Return (`DING-R02`).

Return is transport, not a delivery receipt. After any submission attempt, a
bounded observation loop asks the selected harness adapter for one of five
states:

| Receipt state | Meaning |
| --- | --- |
| `Accepted` | The expected notice text is visible in an adapter-recognized submitted-prompt or queued-message pattern while the lowest live composer is empty or an accepted idle placeholder |
| `RetainedSafe` | The exact notice remains the complete live composer and Return is currently safe |
| `RetainedBlocked` | The exact notice remains the complete live composer but the harness is active or blocked |
| `NotRetained` | A maintained adapter parsed the live composer and positively proved that the exact notice is neither its complete contents nor an accepted submission |
| `Unproven` | No positive acceptance or exact retained-composer state was proven |

Only `Accepted` becomes `Delivered` (`DING-R10`). PTY command success, generic
screen change, disappearance alone, a changed composer, unreadable output, and
observation timeout never become delivery. `NotRetained` requires successful
parsing by a maintained adapter; missing or unrecognized composer evidence stays
`Unproven`. A staged retry completes without input when it observes `Accepted`;
it may send one bare Return only after two adjacent `RetainedSafe` observations,
then must obtain the same positive receipt. `NotRetained` releases ownership
only when the notice is already archived; an unread notice remains staged.
`RetainedBlocked`, `NotRetained`, and `Unproven` send no input. No retry
re-pastes.

## Harness dispatch

Each maintained harness implements one interface: locate its composer on a
screen, reporting the row it occupies, and classify that screen against the
expected notice. Only the row crosses the interface — the located composer's
contents stay inside the harness that found them, so the registry never carries
a harness-specific payload and stays a uniform list. A registry enumerates the
maintained harnesses; adding one is a new module and a new registry entry, with
no edit to shared classification.

Dispatch is positional, not preferential. Every registered harness is asked to
locate a composer, and the one **lowest on the screen** is classified
(`DING-R04`). Scrollback is above the live composer by construction, so this
resolves harness-shaped transcript text — a captured screen from another harness
pasted into a pane — without a per-pair special case. When exactly one harness
locates a composer, that one is classified; when none do, the screen is
`Ambiguous`.

Post-submit receipt classification uses that same positional dispatch and a
shared `ReceiptState` type. Each adapter owns the renderer-specific proof that
the exact notice is retained or accepted; shared delivery code never matches
Codex or Claude pixels directly (`DING-R06`, `DING-R10`).

Positions are compared in one unit. Harnesses do not agree on what they match
against: some locate against the raw screen including escape sequences, others
against ANSI-stripped lines. A raw byte offset and a stripped line index are not
comparable, so each harness reports its position converted to a screen row, and
the comparison is over rows.

## Ownership of vocabulary

Shared code owns only what is common to every harness: the composer states, the
dispatch, ANSI stripping, soft-wrap candidate enumeration, and screen shapes
that are not specific to any harness.

Each harness owns its own composer markers, footer chrome, idle proof, and
active-turn and modal detection (`DING-R06`). Active-turn detection is
deliberately not shared: interrupt hints, progress indicators, and modal
prompts are rendering details of one harness, and applying one harness's strings
to another's pane makes the safety gate depend on a screen the other harness
never draws. Some near-identical strings are therefore duplicated across
harnesses, which is the intended cost.

Widening what counts as blocked is the fail-closed direction and is always safe.
Widening what counts as idle is not, and requires evidence from a real screen.

## Soft-wrap candidates

A composer may wrap a notice across rows. At each renderer-proven wrap boundary
the original text either lost one inter-word space or split a token, so each
boundary yields exactly two candidates. Comparison against the expected notice
succeeds if any candidate matches exactly. The bounded notice length keeps the
candidate set small; an unfamiliar multiline shape yields no match and fails
closed: it is `Ambiguous` before submission and `Unproven` after transport,
never positive `NotRetained` evidence.

## Retry and suppression

Deferred notices retain FIFO order and retry on a bounded backoff, so an
indefinitely occupied composer cannot spawn a terminal probe per inbox poll
(`DING-R08`). A staged archived head advances FIFO only after a maintained
adapter positively observes `NotRetained`; an unread head does not release.
Archive receipts remove pending notices that do not already own a transport.
Declared `busy` never suppresses delivery; only fresh `dnd` defers it
(`DING-R09`).

## Known limits

- Idle proof depends on footer chrome that a harness may render differently
  across permission or approval modes. A harness whose footer is not recognized
  in a given mode defers indefinitely rather than delivering. This is an
  explicit limit per `T01`, and each harness spec states which modes it proves.
- The classifier is a measured heuristic over rendered text, not an evented
  signal, so a renderer change can defer delivery until the grammar is updated.
  Tracked as `DQ2` in [`../spec.md`](../spec.md).
