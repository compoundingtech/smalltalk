# DELTA-005: `costUsd` is harness-reported at its reported scope

Status: resolved — HC-R16 wording confirmed by Johannes (axe Q168, 2026-10-07 ~20:45Z)

## Divergence

HC-R16 originally required "session cost", contradicting the values pi and omp
report. Johannes confirmed the amendment to "the harness-reported cost at the
scope the harness reports, stated per harness in the spec producer table"
(axe Q168, 2026-10-07 ~20:45Z); the accepted requirement now matches the
producer scope and this delta is resolved.

The pi and omp producers shipped on 2026-08-29 publish the **last assistant
message's** `usage.cost.total`, which is what those two harnesses actually
report.

The other three harnesses are not affected: Claude's `cost.total_cost_usd` and
OpenCode's `session.info.cost` genuinely are session totals, and Codex reports no
cost at all.

## VRS

The producer table already named per-message `usage.cost.total` for pi and omp.
The confirmed HC-R16 wording allows harness-specific scope and makes the
requirement and producer rows consistent; the pi and omp producer sections
describe the actual published values.

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

Resolution: HC-R16 and the spec field description now state that cost is
harness-reported at the scope the harness reports, stated per harness in the
producer table. Johannes confirmed the protected requirement change in axe Q168
(2026-10-07 ~20:45Z). Consumers must read the `harness` discriminator before
comparing this field across harnesses — already required for `usedTokens`, whose
meaning differs between pi and omp for unrelated reasons (HC-T03).
