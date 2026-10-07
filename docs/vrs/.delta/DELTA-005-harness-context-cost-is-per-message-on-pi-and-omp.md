# DELTA-005: `costUsd` is harness-reported at its reported scope

Status: open

## Divergence

Proposed HC-R16 says the record carries "the harness-reported cost at the scope
the harness reports". The current accepted requirement instead says "session
cost", creating a contradiction with pi and omp's reported values. **This
requirement change is a proposal only: Johannes must confirm it before HC-R16
is treated as ratified with this wording.**

The pi and omp producers shipped on 2026-08-29 publish the **last assistant
message's** `usage.cost.total`, which is what those two harnesses actually
report.

The other three harnesses are not affected: Claude's `cost.total_cost_usd` and
OpenCode's `session.info.cost` genuinely are session totals, and Codex reports no
cost at all.

## VRS

HC-R16 currently names a session cost, while the producer table already names
per-message `usage.cost.total` for pi and omp. This proposal makes the scope
harness-specific and the rows consistent with it; the pi and omp producer
sections describe the actual published values.

## Implementation

Turning pi's or omp's per-message cost into a session total means summing every
message's `usage.cost.total` in the producer. That is precisely the
producer-side accumulator HC-R16 already refuses one field over, for
`sessionTotalTokens`, and the reasoning transfers unchanged: the sum's
correctness depends on having observed every message, an extension loaded into a
session that is already running has not, and a half-observed total is a worse
answer than an honest smaller one. Nothing else in st2 reconciles this number —
HC-R16's "carried as what the harness reported and nothing more" is the binding
half of the requirement, and a fabricated sum would violate it in order to
satisfy the word "session".

The alternative — writing `null` — was rejected because the per-message figure
is real, is what pi and omp show their own operators, and is strictly more
information than nothing.

One consequence is load-bearing in the implementation and is documented in the
producer sections: because the record replaces a reading's fields wholesale,
a frame emitted from an event that carries no cost would *erase* the published
one. The extension holds the last assistant cost and restates it on every frame,
and clears the hold on session replacement.

## Direction

update VRS

## Resolution Signal

Proposed resolution: amend HC-R16 so adjacent cost is "the harness-reported
cost at the scope the harness reports", and align the spec field description and
producer table. **Johannes must confirm this amendment** because requirements.md
is protected. The spec and this delta record can describe the proposed resolution
without that ratification. Consumers must read the `harness` discriminator before
comparing this field across harnesses — already required for `usedTokens`, whose
meaning differs between pi and omp for unrelated reasons (HC-T03).
